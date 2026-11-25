# apps/navi/scripts/test_project_review_diff.py
"""Unit tests for apps/navi/workers/project_review_diff.py."""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest

HELPER_PATH = os.path.abspath(
    os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..",
        "workers",
        "project_review_diff.py",
    )
)

REQUEST_SCHEMA = "navi-review-preview-request-v1"
REPLY_SCHEMA = "navi-review-preview-v1"
REVIEW_ID = "review-" + ("a" * 32)
DIGEST = "b" * 64

_STUB_SITECUSTOMIZE = r"""
import json, os, sys, types

if os.environ.get("NAVI_TEST_INVENTORY_STUB") == "1":
    mod = types.ModuleType("project_review_inventory")
    mod.FIXED_REVIEW_FILES = ("before.txt", "proposed-after.txt", "manifest.json")
    mod.MAX_REVIEW_FILE_BYTES = 1024 * 1024
    mod.PROPOSAL_REVIEWS_DIR = "proposal-reviews"

    def _open_existing_review_storage(data_root, *, cancel=None, deadline=None):
        path = os.path.join(data_root, mod.PROPOSAL_REVIEWS_DIR)
        flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        if hasattr(os, "O_DIRECTORY"):
            flags |= os.O_DIRECTORY
        try:
            return os.open(path, flags)
        except FileNotFoundError:
            return None

    def inspect_pinned_review_workspace(
        storage_fd, review_id, *, storage_dev, cancel=None, deadline=None
    ):
        dir_flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        if hasattr(os, "O_DIRECTORY"):
            dir_flags |= os.O_DIRECTORY
        child_fd = os.open(review_id, dir_flags, dir_fd=storage_fd)
        try:
            meta_fd = os.open(
                "manifest.json",
                os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=child_fd,
            )
            try:
                chunks = []
                while True:
                    chunk = os.read(meta_fd, 65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
                return json.loads(b"".join(chunks).decode("utf-8"))
            finally:
                os.close(meta_fd)
        finally:
            os.close(child_fd)

    mod._open_existing_review_storage = _open_existing_review_storage
    mod.inspect_pinned_review_workspace = inspect_pinned_review_workspace
    sys.modules["project_review_inventory"] = mod
"""


def _sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def _snapshot_tree(root):
    entries = {}
    for base, dirs, files in os.walk(root):
        dirs.sort()
        files.sort()
        for name in dirs + files:
            full = os.path.join(base, name)
            st = os.lstat(full)
            rel = os.path.relpath(full, root)
            entries[rel] = (st.st_mode, st.st_ino, st.st_size, st.st_mtime_ns)
    return entries


class ProjectReviewDiffTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name
        self.stub_dir = os.path.join(self.root, "stub")
        self.data_root = os.path.join(self.root, "data")
        self.workspace_root = os.path.join(self.root, "workspace")
        os.makedirs(self.stub_dir, mode=0o700)
        os.makedirs(self.data_root, mode=0o700)
        os.makedirs(self.workspace_root, mode=0o700)
        with open(
            os.path.join(self.stub_dir, "sitecustomize.py"),
            "w",
            encoding="utf-8",
        ) as fh:
            fh.write(_STUB_SITECUSTOMIZE)

    def tearDown(self):
        self._tmp.cleanup()

    def _write_review(
        self,
        *,
        review_id=REVIEW_ID,
        proposal_digest=DIGEST,
        original_path="src/app.py",
        before_bytes=b"alpha\nbeta\n",
        after_bytes=b"alpha\ngamma\n",
        workspace_bytes=b"alpha\nbeta\n",
    ):
        storage_dir = os.path.join(self.data_root, "proposal-reviews")
        review_dir = os.path.join(storage_dir, review_id)
        os.makedirs(storage_dir, mode=0o700, exist_ok=True)
        os.chmod(storage_dir, 0o700)
        os.makedirs(review_dir, mode=0o700, exist_ok=True)
        os.chmod(review_dir, 0o700)

        before_path = os.path.join(review_dir, "before.txt")
        after_path = os.path.join(review_dir, "proposed-after.txt")
        meta_path = os.path.join(review_dir, "manifest.json")

        for path, data in ((before_path, before_bytes), (after_path, after_bytes)):
            with open(path, "wb") as fh:
                fh.write(data)
            os.chmod(path, 0o600)

        record = {
            "valid": True,
            "status": "valid",
            "applied": False,
            "review_id": review_id,
            "files": ["before.txt", "proposed-after.txt", "manifest.json"],
            "proposal_digest": proposal_digest,
            "original_path": original_path,
            "before_sha256": _sha256_hex(before_bytes),
            "after_sha256": _sha256_hex(after_bytes),
            "before_bytes": len(before_bytes),
            "after_bytes": len(after_bytes),
        }
        with open(meta_path, "wb") as fh:
            fh.write(json.dumps(record).encode("utf-8"))
        os.chmod(meta_path, 0o600)

        ws_target = os.path.join(self.workspace_root, *original_path.split("/"))
        if workspace_bytes is not None:
            os.makedirs(os.path.dirname(ws_target), mode=0o700, exist_ok=True)
            with open(ws_target, "wb") as fh:
                fh.write(workspace_bytes)
            os.chmod(ws_target, 0o600)

        return review_dir, ws_target

    def _run_helper(self, payload, *, raw_input=None, extra_args=()):
        if raw_input is None:
            raw_input = (json.dumps(payload, separators=(",", ":")) + "\n").encode(
                "utf-8"
            )
        env = os.environ.copy()
        env["NAVI_TEST_INVENTORY_STUB"] = "1"
        existing_pp = env.get("PYTHONPATH")
        env["PYTHONPATH"] = (
            f"{self.stub_dir}{os.pathsep}{existing_pp}"
            if existing_pp
            else self.stub_dir
        )
        proc = subprocess.run(
            [sys.executable, HELPER_PATH, *extra_args],
            input=raw_input,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            timeout=15,
            check=False,
        )
        self.assertEqual(proc.stderr, b"")
        self.assertTrue(proc.stdout.endswith(b"\n"))
        self.assertEqual(proc.stdout.count(b"\n"), 1)
        self.assertLessEqual(len(proc.stdout), 40000)
        reply = json.loads(proc.stdout[:-1].decode("utf-8"))
        self.assertEqual(reply["schema"], REPLY_SCHEMA)
        return proc, reply

    def _request(self, *, review_id=REVIEW_ID, proposal_digest=DIGEST):
        return {
            "schema": REQUEST_SCHEMA,
            "data_root": self.data_root,
            "workspace_root": self.workspace_root,
            "review_id": review_id,
            "proposal_digest": proposal_digest,
        }

    def test_valid_fresh_preview_read_only_and_no_paths_leaked(self):
        self._write_review(original_path="secret_dir/target_file.py")
        before_data_tree = _snapshot_tree(self.data_root)
        before_ws_tree = _snapshot_tree(self.workspace_root)

        proc, reply = self._run_helper(self._request())

        self.assertEqual(proc.returncode, 0)
        self.assertTrue(reply["ok"])
        self.assertEqual(reply["review_id"], REVIEW_ID)
        self.assertFalse(reply["stale"])
        self.assertFalse(reply["truncated"])
        self.assertEqual(reply["before_bytes"], len(b"alpha\nbeta\n"))
        self.assertEqual(reply["after_bytes"], len(b"alpha\ngamma\n"))
        self.assertIn("--- before\n+++ after\n", reply["diff"])
        self.assertIn("-beta\n+gamma", reply["diff"])
        self.assertEqual(reply["diff_lines"], reply["diff"].count("\n") + 1)

        raw_stdout = proc.stdout.decode("utf-8")
        for forbidden in (
            self.root,
            self.data_root,
            self.workspace_root,
            "secret_dir",
            "target_file.py",
        ):
            self.assertNotIn(forbidden, raw_stdout)

        self.assertEqual(_snapshot_tree(self.data_root), before_data_tree)
        self.assertEqual(_snapshot_tree(self.workspace_root), before_ws_tree)

    def test_stale_when_workspace_modified_or_missing(self):
        _, ws_target = self._write_review(workspace_bytes=b"alpha\nchanged\n")
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 0)
        self.assertTrue(reply["ok"])
        self.assertTrue(reply["stale"])

        os.unlink(ws_target)
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 0)
        self.assertTrue(reply["ok"])
        self.assertTrue(reply["stale"])

    def test_truncation_by_lines_and_reply_bytes(self):
        before = "".join(f"old-{i}\n" for i in range(450)).encode("utf-8")
        after = "".join(f"new-{i}\n" for i in range(450)).encode("utf-8")
        self._write_review(
            before_bytes=before, after_bytes=after, workspace_bytes=before
        )
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 0)
        self.assertTrue(reply["ok"])
        self.assertTrue(reply["truncated"])
        self.assertLessEqual(reply["diff_lines"], 400)

        ctrl_after = (("\x01" * 10000) + "\n").encode("utf-8")
        self._write_review(
            before_bytes=b"a\n", after_bytes=ctrl_after, workspace_bytes=b"a\n"
        )
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 0)
        self.assertTrue(reply["ok"])
        self.assertTrue(reply["truncated"])
        self.assertLessEqual(len(proc.stdout), 40000)

    def test_digest_mismatch_and_tampered_file_rejected(self):
        review_dir, _ = self._write_review()

        proc, reply = self._run_helper(self._request(proposal_digest="c" * 64))
        self.assertEqual(proc.returncode, 1)
        self.assertFalse(reply["ok"])
        self.assertEqual(reply["reason"], "digest_mismatch")

        before_path = os.path.join(review_dir, "before.txt")
        with open(before_path, "wb") as fh:
            fh.write(b"alpha\ntamp\n")
        os.chmod(before_path, 0o600)

        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 1)
        self.assertFalse(reply["ok"])
        self.assertEqual(reply["reason"], "verification_failed")

    def test_permissions_and_symlinks_rejected(self):
        review_dir, ws_target = self._write_review()
        before_path = os.path.join(review_dir, "before.txt")

        os.chmod(before_path, 0o644)
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_permissions")
        os.chmod(before_path, 0o600)

        decoy = os.path.join(self.root, "decoy.txt")
        with open(decoy, "wb") as fh:
            fh.write(b"alpha\nbeta\n")
        os.chmod(decoy, 0o600)

        os.unlink(before_path)
        os.symlink(decoy, before_path)
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_entry_type")

        os.unlink(before_path)
        with open(before_path, "wb") as fh:
            fh.write(b"alpha\nbeta\n")
        os.chmod(before_path, 0o600)

        os.unlink(ws_target)
        os.symlink(decoy, ws_target)
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_entry_type")

    def test_missing_storage_and_missing_review(self):
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "not_found")

        os.makedirs(os.path.join(self.data_root, "proposal-reviews"), mode=0o700)
        proc, reply = self._run_helper(self._request())
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "not_found")

    def test_invalid_framing_and_cli_args_rejected(self):
        valid_req = self._request()

        proc, reply = self._run_helper(
            None, raw_input=json.dumps(valid_req).encode("utf-8")
        )
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_request")
        self.assertIsNone(reply["review_id"])

        extra_key = dict(valid_req, unexpected=1)
        proc, reply = self._run_helper(extra_key)
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_request")
        self.assertEqual(reply["review_id"], REVIEW_ID)

        dup_raw = (
            '{"schema":"'
            + REQUEST_SCHEMA
            + '","schema":"'
            + REQUEST_SCHEMA
            + '","data_root":"'
            + self.data_root
            + '","workspace_root":"'
            + self.workspace_root
            + '","review_id":"'
            + REVIEW_ID
            + '","proposal_digest":"'
            + DIGEST
            + '"}\n'
        ).encode("utf-8")
        proc, reply = self._run_helper(None, raw_input=dup_raw)
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_request")

        proc, reply = self._run_helper(valid_req, extra_args=("--extra",))
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(reply["reason"], "invalid_request")
        self.assertNotIn(self.root, proc.stdout.decode("utf-8"))


if __name__ == "__main__":
    unittest.main()
