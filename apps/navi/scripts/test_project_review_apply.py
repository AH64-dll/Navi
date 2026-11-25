# apps/navi/scripts/test_project_review_apply.py
"""Unit tests for apps/navi/workers/project_review_apply.py using synthetic temp dirs."""

import hashlib
import io
import json
import os
import stat
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

SCRIPTS_DIR = os.path.dirname(os.path.abspath(__file__))
WORKERS_DIR = os.path.normpath(os.path.join(SCRIPTS_DIR, "..", "workers"))
if WORKERS_DIR not in sys.path:
    sys.path.insert(0, WORKERS_DIR)

import project_review_apply
import project_review_inventory


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class TestProjectReviewApply(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp_root = os.path.realpath(self._tmp.name)
        os.chmod(self.tmp_root, 0o700)

        self.data_root = os.path.join(self.tmp_root, "data-root-secret-dir")
        self.storage_dir = os.path.join(
            self.data_root, project_review_inventory.PROPOSAL_REVIEWS_DIR
        )
        self.workspace_root = os.path.join(self.tmp_root, "workspace-secret-dir")

        self.review_id = "review-0123456789abcdef0123456789abcdef"
        self.proposal_digest = "a" * 64
        self.original_path = "src/example.txt"
        self.before_bytes = b"SECRET_BEFORE_LINE_1\noriginal content\n"
        self.after_bytes = b"SECRET_AFTER_LINE_1\nupdated content\n"
        self.before_sha256 = _sha256_hex(self.before_bytes)
        self.after_sha256 = _sha256_hex(self.after_bytes)

        self.review_dir = os.path.join(self.storage_dir, self.review_id)
        os.makedirs(self.review_dir, mode=0o700)
        os.chmod(self.data_root, 0o700)
        os.chmod(self.storage_dir, 0o700)
        os.chmod(self.review_dir, 0o700)

        self.before_file = os.path.join(self.review_dir, "before.txt")
        self.after_file = os.path.join(self.review_dir, "proposed-after.txt")
        self.manifest_file = os.path.join(self.review_dir, "review.json")

        self._write_mode_0600(self.before_file, self.before_bytes)
        self._write_mode_0600(self.after_file, self.after_bytes)
        manifest_payload = {
            "review_id": self.review_id,
            "status": "valid",
            "applied": False,
            "proposal_digest": self.proposal_digest,
            "files": list(project_review_inventory.FIXED_REVIEW_FILES),
        }
        self._write_mode_0600(
            self.manifest_file,
            json.dumps(manifest_payload, sort_keys=True).encode("utf-8"),
        )

        self.target_dir = os.path.join(self.workspace_root, "src")
        os.makedirs(self.target_dir, mode=0o700)
        os.chmod(self.workspace_root, 0o700)
        os.chmod(self.target_dir, 0o700)

        self.target_file = os.path.join(self.workspace_root, self.original_path)
        self._write_mode_0600(self.target_file, self.before_bytes)

    def tearDown(self):
        self._tmp.cleanup()

    @staticmethod
    def _write_mode_0600(path: str, data: bytes):
        with open(path, "wb") as handle:
            handle.write(data)
        os.chmod(path, 0o600)

    def _valid_record(self, **overrides):
        record = {
            "review_id": self.review_id,
            "valid": True,
            "status": "valid",
            "applied": False,
            "original_path": self.original_path,
            "proposal_digest": self.proposal_digest,
            "before_sha256": self.before_sha256,
            "after_sha256": self.after_sha256,
            "before_bytes": len(self.before_bytes),
            "after_bytes": len(self.after_bytes),
            "files": list(project_review_inventory.FIXED_REVIEW_FILES),
        }
        record.update(overrides)
        return record

    def _read_bytes(self, path: str) -> bytes:
        with open(path, "rb") as handle:
            return handle.read()

    def _assert_review_copy_intact(self):
        self.assertTrue(os.path.isdir(self.review_dir))
        self.assertEqual(
            sorted(os.listdir(self.review_dir)),
            sorted(project_review_inventory.FIXED_REVIEW_FILES),
        )
        self.assertEqual(self._read_bytes(self.before_file), self.before_bytes)
        self.assertEqual(self._read_bytes(self.after_file), self.after_bytes)
        for path in (self.before_file, self.after_file, self.manifest_file):
            self.assertEqual(stat.S_IMODE(os.lstat(path).st_mode), 0o600)

    def _assert_no_leaks(self, reply: dict, extra_forbidden=()):
        encoded = (
            json.dumps(reply, ensure_ascii=True, separators=(",", ":")) + "\n"
        ).encode("ascii")
        self.assertLessEqual(len(encoded), project_review_apply.MAX_REPLY_BYTES)
        self.assertEqual(reply.get("schema"), project_review_apply.REPLY_SCHEMA)
        self.assertTrue(set(reply.keys()).issubset({"schema", "applied", "review_id", "reason"}))

        text = encoded.decode("ascii")
        forbidden = [
            self.tmp_root,
            self.data_root,
            self.workspace_root,
            "data-root-secret-dir",
            "workspace-secret-dir",
            project_review_inventory.PROPOSAL_REVIEWS_DIR,
            self.original_path,
            "example.txt",
            "proposed-after.txt",
            "before.txt",
            "review.json",
            self.proposal_digest,
            self.before_sha256,
            self.after_sha256,
            "SECRET_BEFORE_LINE_1",
            "SECRET_AFTER_LINE_1",
            *extra_forbidden,
        ]
        for token in forbidden:
            self.assertNotIn(token, text)

    def _run_main_raw(self, raw_bytes: bytes):
        out = io.BytesIO()
        code = project_review_apply.main(stdin=io.BytesIO(raw_bytes), stdout=out)
        self.assertEqual(code, 0)
        raw_reply = out.getvalue()
        self.assertTrue(raw_reply.endswith(b"\n"))
        self.assertEqual(raw_reply.count(b"\n"), 1)
        self.assertLessEqual(len(raw_reply), project_review_apply.MAX_REPLY_BYTES)
        text = raw_reply.decode("ascii")
        parsed = json.loads(text)
        return parsed, raw_reply

    def test_success_replaces_target_and_keeps_review_copy(self):
        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ) as inspect_mock:
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        inspect_mock.assert_called_once()
        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": True,
                "review_id": self.review_id,
            },
        )
        self._assert_no_leaks(reply)
        self.assertEqual(self._read_bytes(self.target_file), self.after_bytes)
        self.assertEqual(stat.S_IMODE(os.lstat(self.target_file).st_mode), 0o600)
        self._assert_review_copy_intact()

    def test_digest_mismatch_changes_nothing(self):
        wrong_digest = "b" * 64
        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                wrong_digest,
            )

        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": False,
                "review_id": self.review_id,
                "reason": "digest_mismatch",
            },
        )
        self._assert_no_leaks(reply, extra_forbidden=(wrong_digest,))
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)
        self._assert_review_copy_intact()

    def test_before_mismatch_changes_nothing(self):
        concurrent_edit = b"CONCURRENT_EDIT_SECRET\nchanged target\n"
        self._write_mode_0600(self.target_file, concurrent_edit)

        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": False,
                "review_id": self.review_id,
                "reason": "before_mismatch",
            },
        )
        self._assert_no_leaks(reply, extra_forbidden=("CONCURRENT_EDIT_SECRET",))
        self.assertEqual(self._read_bytes(self.target_file), concurrent_edit)
        self._assert_review_copy_intact()

    def test_after_file_hash_mismatch_same_length_changes_nothing(self):
        tampered = b"X" * len(self.after_bytes)
        self.assertNotEqual(tampered, self.after_bytes)
        self._write_mode_0600(self.after_file, tampered)

        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": False,
                "review_id": self.review_id,
                "reason": "verification_failed",
            },
        )
        self._assert_no_leaks(reply)
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_after_file_length_mismatch_changes_nothing(self):
        self._write_mode_0600(self.after_file, self.after_bytes + b"extra")

        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(reply["applied"], False)
        self.assertEqual(reply["reason"], "verification_failed")
        self._assert_no_leaks(reply)
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_symlinked_proposed_after_refused(self):
        outside_file = os.path.join(self.tmp_root, "outside-after.txt")
        self._write_mode_0600(outside_file, self.after_bytes)
        os.unlink(self.after_file)
        os.symlink(outside_file, self.after_file)

        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": False,
                "review_id": self.review_id,
                "reason": "invalid_entry_type",
            },
        )
        self._assert_no_leaks(reply, extra_forbidden=("outside-after.txt",))
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_non_0600_proposed_after_refused(self):
        os.chmod(self.after_file, 0o644)

        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(reply["applied"], False)
        self.assertEqual(reply["reason"], "invalid_permissions")
        self._assert_no_leaks(reply)
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_hardlinked_proposed_after_refused(self):
        extra_link = os.path.join(self.tmp_root, "hardlink-after.txt")
        os.link(self.after_file, extra_link)

        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(reply["applied"], False)
        self.assertEqual(reply["reason"], "verification_failed")
        self._assert_no_leaks(reply, extra_forbidden=("hardlink-after.txt",))
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_missing_storage_returns_not_found(self):
        missing_data_root = os.path.join(self.tmp_root, "empty-data-root")
        os.makedirs(missing_data_root, mode=0o700)

        reply = project_review_apply.apply_review(
            missing_data_root,
            self.workspace_root,
            self.review_id,
            self.proposal_digest,
        )

        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": False,
                "review_id": self.review_id,
                "reason": "not_found",
            },
        )
        self._assert_no_leaks(reply, extra_forbidden=("empty-data-root",))
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_invalid_record_returns_verification_failed(self):
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value={"review_id": self.review_id, "valid": False, "reason": "corrupt"},
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(reply["applied"], False)
        self.assertEqual(reply["reason"], "verification_failed")
        self._assert_no_leaks(reply)
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_write_source_file_exception_maps_to_verification_failed(self):
        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ), mock.patch.object(
            project_review_apply,
            "write_source_file",
            side_effect=RuntimeError(f"boom {self.target_file} {self.after_sha256}"),
        ):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )

        self.assertEqual(reply["applied"], False)
        self.assertEqual(reply["reason"], "verification_failed")
        self._assert_no_leaks(reply, extra_forbidden=("boom",))

    def test_invalid_request_arguments(self):
        bad_ids = [
            "",
            "review-123",
            "review-" + "A" * 32,
            self.review_id + "\n",
            "../" + self.review_id,
            None,
            12345,
        ]
        for bad_id in bad_ids:
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                bad_id,
                self.proposal_digest,
            )
            self.assertEqual(
                reply,
                {
                    "schema": "navi-review-apply-v1",
                    "applied": False,
                    "review_id": None,
                    "reason": "invalid_request",
                },
            )
            self._assert_no_leaks(reply)

        bad_digests = [
            "",
            "a" * 63,
            "a" * 65,
            "A" * 64,
            "g" * 64,
            self.proposal_digest + "\n",
            None,
        ]
        for bad_digest in bad_digests:
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                bad_digest,
            )
            self.assertEqual(
                reply,
                {
                    "schema": "navi-review-apply-v1",
                    "applied": False,
                    "review_id": self.review_id,
                    "reason": "invalid_request",
                },
            )
            self._assert_no_leaks(reply)

        for bad_root in ("", "relative/path", self.data_root + "\x00bad", None):
            reply = project_review_apply.apply_review(
                bad_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
            )
            self.assertEqual(reply["applied"], False)
            self.assertEqual(reply["reason"], "invalid_request")
            self._assert_no_leaks(reply)

        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_main_valid_request_and_no_leaks(self):
        frame = {
            "schema": "navi-review-apply-request-v1",
            "data_root": self.data_root,
            "workspace_root": self.workspace_root,
            "review_id": self.review_id,
            "proposal_digest": self.proposal_digest,
        }
        record = self._valid_record()
        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            return_value=record,
        ):
            parsed, _ = self._run_main_raw(json.dumps(frame).encode("utf-8"))

        self.assertEqual(
            parsed,
            {
                "schema": "navi-review-apply-v1",
                "applied": True,
                "review_id": self.review_id,
            },
        )
        self._assert_no_leaks(parsed)
        self.assertEqual(self._read_bytes(self.target_file), self.after_bytes)
        self._assert_review_copy_intact()

    def test_main_rejects_invalid_request_shapes(self):
        base = {
            "schema": "navi-review-apply-request-v1",
            "data_root": self.data_root,
            "workspace_root": self.workspace_root,
            "review_id": self.review_id,
            "proposal_digest": self.proposal_digest,
        }
        valid_bytes = json.dumps(base).encode("utf-8")

        wrong_schema = dict(base, schema="wrong-schema-v1")
        missing_key = {k: v for k, v in base.items() if k != "proposal_digest"}
        extra_key = dict(base, extra="nope")

        duplicate_key_raw = (
            '{"schema":"navi-review-apply-request-v1",'
            f'"data_root":{json.dumps(self.data_root)},'
            f'"workspace_root":{json.dumps(self.workspace_root)},'
            f'"review_id":{json.dumps(self.review_id)},'
            f'"review_id":{json.dumps(self.review_id)},'
            f'"proposal_digest":{json.dumps(self.proposal_digest)}}}'
        ).encode("utf-8")

        nan_raw = (
            '{"schema":"navi-review-apply-request-v1",'
            f'"data_root":{json.dumps(self.data_root)},'
            f'"workspace_root":{json.dumps(self.workspace_root)},'
            f'"review_id":{json.dumps(self.review_id)},'
            '"proposal_digest":NaN}'
        ).encode("utf-8")

        inf_raw = (
            '{"schema":"navi-review-apply-request-v1",'
            f'"data_root":{json.dumps(self.data_root)},'
            f'"workspace_root":{json.dumps(self.workspace_root)},'
            f'"review_id":{json.dumps(self.review_id)},'
            '"proposal_digest":Infinity}'
        ).encode("utf-8")

        neg_inf_raw = (
            '{"schema":"navi-review-apply-request-v1",'
            f'"data_root":{json.dumps(self.data_root)},'
            f'"workspace_root":{json.dumps(self.workspace_root)},'
            f'"review_id":{json.dumps(self.review_id)},'
            '"proposal_digest":-Infinity}'
        ).encode("utf-8")

        oversized_raw = b" " * (project_review_apply.MAX_REQUEST_BYTES + 1)

        bad_payloads = [
            b"",
            b"\xff\xfe",
            b"[]",
            json.dumps(wrong_schema).encode("utf-8"),
            json.dumps(missing_key).encode("utf-8"),
            json.dumps(extra_key).encode("utf-8"),
            duplicate_key_raw,
            nan_raw,
            inf_raw,
            neg_inf_raw,
            oversized_raw,
        ]

        for payload in bad_payloads:
            parsed, _ = self._run_main_raw(payload)
            self.assertEqual(parsed["applied"], False)
            self.assertEqual(parsed["reason"], "invalid_request")
            self.assertIsNone(parsed["review_id"])
            self._assert_no_leaks(parsed)

        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_cancel_event_and_callable_and_midflight(self):
        event = threading.Event()
        event.set()
        reply = project_review_apply.apply_review(
            self.data_root,
            self.workspace_root,
            self.review_id,
            self.proposal_digest,
            cancel=event,
        )
        self.assertEqual(
            reply,
            {
                "schema": "navi-review-apply-v1",
                "applied": False,
                "review_id": self.review_id,
                "reason": "canceled",
            },
        )
        self._assert_no_leaks(reply)

        reply_callable = project_review_apply.apply_review(
            self.data_root,
            self.workspace_root,
            self.review_id,
            self.proposal_digest,
            cancel=lambda: True,
        )
        self.assertEqual(reply_callable["applied"], False)
        self.assertEqual(reply_callable["reason"], "canceled")
        self._assert_no_leaks(reply_callable)

        mid_event = threading.Event()

        def _inspect_and_cancel(*args, **kwargs):
            mid_event.set()
            return self._valid_record()

        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            side_effect=_inspect_and_cancel,
        ):
            reply_mid = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
                cancel=mid_event,
            )

        self.assertEqual(reply_mid["applied"], False)
        self.assertEqual(reply_mid["reason"], "canceled")
        self._assert_no_leaks(reply_mid)
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)

    def test_deadline_expired_and_midflight_timeout(self):
        for timeout_val in (0.0, -1.0):
            reply = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
                timeout_seconds=timeout_val,
            )
            self.assertEqual(
                reply,
                {
                    "schema": "navi-review-apply-v1",
                    "applied": False,
                    "review_id": self.review_id,
                    "reason": "timed_out",
                },
            )
            self._assert_no_leaks(reply)

        def _slow_inspect(*args, **kwargs):
            time.sleep(0.06)
            return self._valid_record()

        with mock.patch.object(
            project_review_apply,
            "inspect_pinned_review_workspace",
            side_effect=_slow_inspect,
        ):
            reply_mid = project_review_apply.apply_review(
                self.data_root,
                self.workspace_root,
                self.review_id,
                self.proposal_digest,
                timeout_seconds=0.02,
            )

        self.assertEqual(reply_mid["applied"], False)
        self.assertEqual(reply_mid["reason"], "timed_out")
        self._assert_no_leaks(reply_mid)
        self.assertEqual(self._read_bytes(self.target_file), self.before_bytes)


if __name__ == "__main__":
    unittest.main()
