# apps/navi/workers/project_review_apply.py
"""Owner-only, default-off helper to apply one validated private review copy.

Stdlib only, no process spawning, no network. Does not delete the review copy
after applying.
"""

import errno
import hashlib
import hmac
import json
import math
import os
import re
import stat
import sys
import time

WORKERS_DIR = os.path.dirname(os.path.abspath(__file__))
if WORKERS_DIR not in sys.path:
    sys.path.insert(0, WORKERS_DIR)

import project_review_inventory
import project_source_write
from project_review_inventory import (
    FIXED_REVIEW_FILES,
    MAX_REVIEW_FILE_BYTES,
    PROPOSAL_REVIEWS_DIR,
    _open_existing_review_storage,
    inspect_pinned_review_workspace,
)
from project_source_write import write_source_file

REQUEST_SCHEMA = "navi-review-apply-request-v1"
REPLY_SCHEMA = "navi-review-apply-v1"

MAX_REQUEST_BYTES = 65536
MAX_REPLY_BYTES = 512
READ_CHUNK_BYTES = 65536

AFTER_FILENAME = "proposed-after.txt"
REQUIRED_REVIEW_FILE_MODE = 0o600

REVIEW_ID_RE = re.compile(r"^review-[0-9a-f]{32}\Z")
SHA256_HEX_RE = re.compile(r"^[0-9a-f]{64}\Z")

EXPECTED_REQUEST_KEYS = frozenset(
    {
        "schema",
        "data_root",
        "workspace_root",
        "review_id",
        "proposal_digest",
    }
)

REASON_INVALID_REQUEST = "invalid_request"
REASON_NOT_FOUND = "not_found"
REASON_DIGEST_MISMATCH = "digest_mismatch"
REASON_VERIFICATION_FAILED = "verification_failed"
REASON_BEFORE_MISMATCH = "before_mismatch"
REASON_INVALID_ENTRY_TYPE = "invalid_entry_type"
REASON_INVALID_PERMISSIONS = "invalid_permissions"
REASON_TOO_LARGE = "too_large"
REASON_DIRECTORY_CHANGED = "directory_changed"
REASON_INVALID_STORAGE = "invalid_storage"
REASON_CANCELED = "canceled"
REASON_TIMED_OUT = "timed_out"

ALLOWED_REASONS = frozenset(
    {
        REASON_INVALID_REQUEST,
        REASON_NOT_FOUND,
        REASON_DIGEST_MISMATCH,
        REASON_VERIFICATION_FAILED,
        REASON_BEFORE_MISMATCH,
        REASON_INVALID_ENTRY_TYPE,
        REASON_INVALID_PERMISSIONS,
        REASON_TOO_LARGE,
        REASON_DIRECTORY_CHANGED,
        REASON_INVALID_STORAGE,
        REASON_CANCELED,
        REASON_TIMED_OUT,
    }
)

_ORIG_OPEN_EXISTING_REVIEW_STORAGE = _open_existing_review_storage
_ORIG_INSPECT_PINNED_REVIEW_WORKSPACE = inspect_pinned_review_workspace
_ORIG_WRITE_SOURCE_FILE = write_source_file


class _ApplyFailure(Exception):
    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


def _safe_review_id(review_id):
    if isinstance(review_id, str) and REVIEW_ID_RE.fullmatch(review_id):
        return review_id
    return None


def _is_sha256_hex(value):
    return isinstance(value, str) and bool(SHA256_HEX_RE.fullmatch(value))


def _validate_root_path(value):
    if isinstance(value, os.PathLike):
        value = os.fspath(value)
    if not isinstance(value, str) or not value or "\x00" in value:
        return None
    if not os.path.isabs(value):
        return None
    return value


def _validate_relative_posix_path(path):
    if not isinstance(path, str) or not path:
        return False
    if "\x00" in path or "\\" in path:
        return False
    if path.startswith("/") or path.endswith("/"):
        return False
    parts = path.split("/")
    if any(part in ("", ".", "..") for part in parts):
        return False
    return True


def _success(review_id):
    return {
        "schema": REPLY_SCHEMA,
        "applied": True,
        "review_id": review_id,
    }


def _failure(review_id, reason):
    safe_reason = reason if reason in ALLOWED_REASONS else REASON_VERIFICATION_FAILED
    return {
        "schema": REPLY_SCHEMA,
        "applied": False,
        "review_id": _safe_review_id(review_id),
        "reason": safe_reason,
    }


def _safe_close(fd):
    try:
        os.close(fd)
    except OSError:
        pass


def _file_snapshot(st):
    return (
        st.st_dev,
        st.st_ino,
        st.st_size,
        st.st_mtime_ns,
        st.st_ctime_ns,
        st.st_nlink,
        st.st_mode,
        st.st_uid,
        st.st_gid,
    )


def _is_canceled(cancel):
    if cancel is None:
        return False
    is_set = getattr(cancel, "is_set", None)
    if callable(is_set):
        return bool(is_set())
    if callable(cancel):
        return bool(cancel())
    raise _ApplyFailure(REASON_INVALID_REQUEST)


def _is_timed_out(deadline):
    return deadline is not None and time.monotonic() >= deadline


def _check_cancel_or_deadline(cancel, deadline):
    if _is_canceled(cancel):
        raise _ApplyFailure(REASON_CANCELED)
    if _is_timed_out(deadline):
        raise _ApplyFailure(REASON_TIMED_OUT)


def _resolve_deadline(timeout_seconds, deadline):
    if deadline is not None:
        if isinstance(deadline, bool) or not isinstance(deadline, (int, float)):
            raise _ApplyFailure(REASON_INVALID_REQUEST)
        if not math.isfinite(deadline):
            raise _ApplyFailure(REASON_INVALID_REQUEST)
        return float(deadline)
    if isinstance(timeout_seconds, bool) or not isinstance(timeout_seconds, (int, float)):
        raise _ApplyFailure(REASON_INVALID_REQUEST)
    if not math.isfinite(timeout_seconds):
        raise _ApplyFailure(REASON_INVALID_REQUEST)
    return time.monotonic() + float(timeout_seconds)


def _map_external_exception(error, *, cancel, deadline, default_reason):
    try:
        if _is_canceled(cancel):
            return REASON_CANCELED
    except _ApplyFailure:
        return REASON_INVALID_REQUEST
    if _is_timed_out(deadline):
        return REASON_TIMED_OUT
    reason = getattr(error, "reason", None)
    if isinstance(reason, str) and reason in ALLOWED_REASONS:
        return reason
    if isinstance(error, FileNotFoundError):
        return REASON_NOT_FOUND
    if isinstance(error, PermissionError):
        return REASON_INVALID_PERMISSIONS
    if isinstance(error, OSError):
        if error.errno in (errno.ELOOP, errno.ENOTDIR, errno.EISDIR):
            return REASON_INVALID_ENTRY_TYPE
        return default_reason
    return default_reason


def _call_open_existing_review_storage(data_root, *, cancel, deadline):
    fn = (
        _open_existing_review_storage
        if _open_existing_review_storage is not _ORIG_OPEN_EXISTING_REVIEW_STORAGE
        else project_review_inventory._open_existing_review_storage
    )
    return fn(data_root, cancel=cancel, deadline=deadline)


def _call_inspect_pinned_review_workspace(
    storage_fd, review_id, *, storage_dev, cancel, deadline
):
    fn = (
        inspect_pinned_review_workspace
        if inspect_pinned_review_workspace is not _ORIG_INSPECT_PINNED_REVIEW_WORKSPACE
        else project_review_inventory.inspect_pinned_review_workspace
    )
    return fn(
        storage_fd,
        review_id,
        storage_dev=storage_dev,
        cancel=cancel,
        deadline=deadline,
    )


def _call_write_source_file(
    workspace_root,
    relative_path,
    expected_before_sha256,
    new_bytes,
    new_sha256,
    *,
    cancel,
    deadline,
):
    fn = (
        write_source_file
        if write_source_file is not _ORIG_WRITE_SOURCE_FILE
        else project_source_write.write_source_file
    )
    return fn(
        workspace_root,
        relative_path,
        expected_before_sha256,
        new_bytes,
        new_sha256,
        cancel=cancel,
        deadline=deadline,
    )


def _validate_record(record, review_id, proposal_digest):
    if not isinstance(record, dict):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)

    if record.get("valid") is not True:
        raw_reason = record.get("reason")
        if raw_reason in (REASON_CANCELED, REASON_TIMED_OUT):
            raise _ApplyFailure(raw_reason)
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)

    if (
        record.get("status") != "valid"
        or record.get("applied") is not False
        or record.get("review_id") != review_id
    ):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)

    files = record.get("files")
    if (
        not isinstance(files, (list, tuple))
        or len(files) != len(FIXED_REVIEW_FILES)
        or not all(isinstance(name, str) for name in files)
        or set(files) != set(FIXED_REVIEW_FILES)
    ):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)

    record_digest = record.get("proposal_digest")
    if not _is_sha256_hex(record_digest):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)

    original_path = record.get("original_path")
    before_sha256 = record.get("before_sha256")
    after_sha256 = record.get("after_sha256")
    before_bytes = record.get("before_bytes")
    after_bytes = record.get("after_bytes")

    if not _validate_relative_posix_path(original_path):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)
    if not _is_sha256_hex(before_sha256) or not _is_sha256_hex(after_sha256):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)
    if (
        isinstance(before_bytes, bool)
        or not isinstance(before_bytes, int)
        or before_bytes < 0
        or before_bytes > MAX_REVIEW_FILE_BYTES
    ):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)
    if (
        isinstance(after_bytes, bool)
        or not isinstance(after_bytes, int)
        or after_bytes < 0
        or after_bytes > MAX_REVIEW_FILE_BYTES
    ):
        raise _ApplyFailure(REASON_VERIFICATION_FAILED)

    if not hmac.compare_digest(record_digest, proposal_digest):
        raise _ApplyFailure(REASON_DIGEST_MISMATCH)

    return original_path, before_sha256, after_sha256, after_bytes


def _inspect_review(data_root, review_id, *, uid, cancel, deadline):
    storage_fd = None
    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_fd = _call_open_existing_review_storage(
                data_root, cancel=cancel, deadline=deadline
            )
        except _ApplyFailure:
            raise
        except Exception as error:
            raise _ApplyFailure(
                _map_external_exception(
                    error,
                    cancel=cancel,
                    deadline=deadline,
                    default_reason=REASON_INVALID_STORAGE,
                )
            ) from error

        if storage_fd is None:
            raise _ApplyFailure(REASON_NOT_FOUND)
        if isinstance(storage_fd, bool) or not isinstance(storage_fd, int) or storage_fd < 0:
            storage_fd = None
            raise _ApplyFailure(REASON_INVALID_STORAGE)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_st = os.fstat(storage_fd)
        except OSError as error:
            raise _ApplyFailure(REASON_INVALID_STORAGE) from error

        if not stat.S_ISDIR(storage_st.st_mode):
            raise _ApplyFailure(REASON_INVALID_ENTRY_TYPE)
        if storage_st.st_uid != uid:
            raise _ApplyFailure(REASON_INVALID_PERMISSIONS)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            record = _call_inspect_pinned_review_workspace(
                storage_fd,
                review_id,
                storage_dev=storage_st.st_dev,
                cancel=cancel,
                deadline=deadline,
            )
        except _ApplyFailure:
            raise
        except Exception as error:
            raise _ApplyFailure(
                _map_external_exception(
                    error,
                    cancel=cancel,
                    deadline=deadline,
                    default_reason=REASON_VERIFICATION_FAILED,
                )
            ) from error

        _check_cancel_or_deadline(cancel, deadline)
        return record, (storage_st.st_dev, storage_st.st_ino)
    finally:
        if storage_fd is not None:
            fd_to_close, storage_fd = storage_fd, None
            _safe_close(fd_to_close)


def _read_pinned_after_bytes(
    data_root,
    review_id,
    expected_after_sha256,
    expected_after_bytes,
    expected_storage_id,
    *,
    uid,
    cancel,
    deadline,
):
    storage_fd = None
    child_fd = None
    after_fd = None
    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_fd = _call_open_existing_review_storage(
                data_root, cancel=cancel, deadline=deadline
            )
        except _ApplyFailure:
            raise
        except Exception as error:
            raise _ApplyFailure(
                _map_external_exception(
                    error,
                    cancel=cancel,
                    deadline=deadline,
                    default_reason=REASON_INVALID_STORAGE,
                )
            ) from error

        if storage_fd is None:
            raise _ApplyFailure(REASON_DIRECTORY_CHANGED)
        if isinstance(storage_fd, bool) or not isinstance(storage_fd, int) or storage_fd < 0:
            storage_fd = None
            raise _ApplyFailure(REASON_INVALID_STORAGE)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_st = os.fstat(storage_fd)
        except OSError as error:
            raise _ApplyFailure(REASON_INVALID_STORAGE) from error

        if not stat.S_ISDIR(storage_st.st_mode):
            raise _ApplyFailure(REASON_INVALID_ENTRY_TYPE)
        if storage_st.st_uid != uid:
            raise _ApplyFailure(REASON_INVALID_PERMISSIONS)
        if (storage_st.st_dev, storage_st.st_ino) != expected_storage_id:
            raise _ApplyFailure(REASON_DIRECTORY_CHANGED)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            child_fd = os.open(
                review_id,
                os.O_RDONLY
                | os.O_DIRECTORY
                | os.O_NOFOLLOW
                | os.O_NONBLOCK
                | os.O_CLOEXEC,
                dir_fd=storage_fd,
            )
        except FileNotFoundError as error:
            raise _ApplyFailure(REASON_DIRECTORY_CHANGED) from error
        except PermissionError as error:
            raise _ApplyFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            if error.errno in (errno.ELOOP, errno.ENOTDIR):
                raise _ApplyFailure(REASON_INVALID_ENTRY_TYPE) from error
            raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error

        _check_cancel_or_deadline(cancel, deadline)
        try:
            child_st = os.fstat(child_fd)
        except OSError as error:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISDIR(child_st.st_mode):
            raise _ApplyFailure(REASON_INVALID_ENTRY_TYPE)
        if child_st.st_uid != uid or child_st.st_dev != storage_st.st_dev:
            raise _ApplyFailure(REASON_INVALID_PERMISSIONS)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            after_fd = os.open(
                AFTER_FILENAME,
                os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=child_fd,
            )
        except FileNotFoundError as error:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error
        except PermissionError as error:
            raise _ApplyFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            if error.errno in (errno.ELOOP, errno.EISDIR, errno.ENOTDIR):
                raise _ApplyFailure(REASON_INVALID_ENTRY_TYPE) from error
            raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error

        _check_cancel_or_deadline(cancel, deadline)
        try:
            before_st = os.fstat(after_fd)
        except OSError as error:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISREG(before_st.st_mode):
            raise _ApplyFailure(REASON_INVALID_ENTRY_TYPE)
        if (
            before_st.st_uid != uid
            or before_st.st_dev != storage_st.st_dev
            or stat.S_IMODE(before_st.st_mode) != REQUIRED_REVIEW_FILE_MODE
        ):
            raise _ApplyFailure(REASON_INVALID_PERMISSIONS)
        if before_st.st_nlink != 1:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED)
        if before_st.st_size > MAX_REVIEW_FILE_BYTES:
            raise _ApplyFailure(REASON_TOO_LARGE)
        if before_st.st_size != expected_after_bytes:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED)

        data = bytearray()
        hasher = hashlib.sha256()
        while True:
            _check_cancel_or_deadline(cancel, deadline)
            remaining = MAX_REVIEW_FILE_BYTES + 1 - len(data)
            if remaining <= 0:
                raise _ApplyFailure(REASON_TOO_LARGE)
            to_read = min(READ_CHUNK_BYTES, remaining)
            try:
                chunk = os.read(after_fd, to_read)
            except OSError as error:
                raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error
            _check_cancel_or_deadline(cancel, deadline)
            if not chunk:
                break
            data.extend(chunk)
            hasher.update(chunk)
            if len(data) > MAX_REVIEW_FILE_BYTES:
                raise _ApplyFailure(REASON_TOO_LARGE)

        try:
            after_st = os.fstat(after_fd)
        except OSError as error:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED) from error

        if (
            len(data) != after_st.st_size
            or _file_snapshot(before_st) != _file_snapshot(after_st)
        ):
            raise _ApplyFailure(REASON_DIRECTORY_CHANGED)

        if len(data) != expected_after_bytes:
            raise _ApplyFailure(REASON_VERIFICATION_FAILED)
        if not hmac.compare_digest(hasher.hexdigest(), expected_after_sha256):
            raise _ApplyFailure(REASON_VERIFICATION_FAILED)

        return bytes(data)
    finally:
        if after_fd is not None:
            fd_to_close, after_fd = after_fd, None
            _safe_close(fd_to_close)
        if child_fd is not None:
            fd_to_close, child_fd = child_fd, None
            _safe_close(fd_to_close)
        if storage_fd is not None:
            fd_to_close, storage_fd = storage_fd, None
            _safe_close(fd_to_close)


def apply_review(
    data_root,
    workspace_root,
    review_id,
    proposal_digest,
    *,
    cancel=None,
    timeout_seconds=10.0,
    deadline=None,
) -> dict:
    safe_id = _safe_review_id(review_id)
    if safe_id is None or not _is_sha256_hex(proposal_digest):
        return _failure(safe_id, REASON_INVALID_REQUEST)

    clean_data_root = _validate_root_path(data_root)
    clean_workspace_root = _validate_root_path(workspace_root)
    if clean_data_root is None or clean_workspace_root is None:
        return _failure(safe_id, REASON_INVALID_REQUEST)

    try:
        effective_deadline = _resolve_deadline(timeout_seconds, deadline)
        _check_cancel_or_deadline(cancel, effective_deadline)
        uid = os.geteuid()

        record, storage_id = _inspect_review(
            clean_data_root,
            safe_id,
            uid=uid,
            cancel=cancel,
            deadline=effective_deadline,
        )
        original_path, before_sha256, after_sha256, after_bytes = _validate_record(
            record, safe_id, proposal_digest
        )

        new_bytes = _read_pinned_after_bytes(
            clean_data_root,
            safe_id,
            after_sha256,
            after_bytes,
            storage_id,
            uid=uid,
            cancel=cancel,
            deadline=effective_deadline,
        )

        _check_cancel_or_deadline(cancel, effective_deadline)
        try:
            write_result = _call_write_source_file(
                clean_workspace_root,
                original_path,
                before_sha256,
                new_bytes,
                after_sha256,
                cancel=cancel,
                deadline=effective_deadline,
            )
        except _ApplyFailure:
            raise
        except Exception as error:
            _check_cancel_or_deadline(cancel, effective_deadline)
            reason = getattr(error, "reason", None)
            if reason in (REASON_CANCELED, REASON_TIMED_OUT):
                return _failure(safe_id, reason)
            return _failure(safe_id, REASON_VERIFICATION_FAILED)

        if not isinstance(write_result, dict):
            return _failure(safe_id, REASON_VERIFICATION_FAILED)
        if write_result.get("written") is True:
            return _success(safe_id)
        if write_result.get("written") is False:
            raw_reason = write_result.get("reason")
            if isinstance(raw_reason, str) and raw_reason in ALLOWED_REASONS:
                return _failure(safe_id, raw_reason)
            _check_cancel_or_deadline(cancel, effective_deadline)
            return _failure(safe_id, REASON_VERIFICATION_FAILED)
        return _failure(safe_id, REASON_VERIFICATION_FAILED)
    except _ApplyFailure as failure:
        return _failure(safe_id, failure.reason)
    except Exception:
        try:
            if _is_canceled(cancel):
                return _failure(safe_id, REASON_CANCELED)
        except _ApplyFailure:
            return _failure(safe_id, REASON_INVALID_REQUEST)
        return _failure(safe_id, REASON_VERIFICATION_FAILED)


def _reject_json_constant(token):
    raise ValueError(f"disallowed JSON constant: {token}")


def _reject_duplicate_keys(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError("duplicate JSON key")
        obj[key] = value
    return obj


def _read_bounded_stdin(stream, max_bytes=MAX_REQUEST_BYTES):
    """Read exactly one newline-terminated frame (or up to EOF) without waiting for EOF."""
    if not hasattr(stream, "readline"):
        return None
    chunk = stream.readline(max_bytes + 1)
    if isinstance(chunk, str):
        chunk = chunk.encode("utf-8")
    elif isinstance(chunk, (bytes, bytearray, memoryview)):
        chunk = bytes(chunk)
    else:
        return None
    if len(chunk) > max_bytes:
        return None
    return chunk


def _parse_request_frame(raw_bytes):
    if raw_bytes is None or not raw_bytes or len(raw_bytes) > MAX_REQUEST_BYTES:
        return None, None
    try:
        text = raw_bytes.decode("utf-8")
    except UnicodeDecodeError:
        return None, None
    try:
        parsed = json.loads(
            text,
            strict=True,
            parse_constant=_reject_json_constant,
            object_pairs_hook=_reject_duplicate_keys,
        )
    except (ValueError, TypeError):
        return None, None

    if not isinstance(parsed, dict):
        return None, None
    if set(parsed.keys()) != EXPECTED_REQUEST_KEYS:
        return None, None
    if parsed["schema"] != REQUEST_SCHEMA:
        return None, None

    safe_id = _safe_review_id(parsed["review_id"])
    for key in ("data_root", "workspace_root", "review_id", "proposal_digest"):
        if not isinstance(parsed[key], str):
            return None, safe_id
    return parsed, safe_id


def _encode_reply_line(reply):
    if not isinstance(reply, dict) or reply.get("schema") != REPLY_SCHEMA:
        sanitized = _failure(None, REASON_VERIFICATION_FAILED)
    else:
        safe_id = _safe_review_id(reply.get("review_id"))
        if reply.get("applied") is True and safe_id is not None:
            sanitized = _success(safe_id)
        else:
            reason = reply.get("reason")
            if not isinstance(reason, str) or reason not in ALLOWED_REASONS:
                reason = REASON_VERIFICATION_FAILED
            sanitized = _failure(safe_id, reason)

    encoded = (
        json.dumps(sanitized, ensure_ascii=True, separators=(",", ":")) + "\n"
    ).encode("ascii")
    if len(encoded) > MAX_REPLY_BYTES:
        fallback = _failure(None, REASON_VERIFICATION_FAILED)
        encoded = (
            json.dumps(fallback, ensure_ascii=True, separators=(",", ":")) + "\n"
        ).encode("ascii")
    return encoded


def main(stdin=None, stdout=None):
    in_stream = stdin if stdin is not None else getattr(sys.stdin, "buffer", sys.stdin)
    out_stream = stdout if stdout is not None else getattr(sys.stdout, "buffer", sys.stdout)
    try:
        raw_bytes = _read_bounded_stdin(in_stream, MAX_REQUEST_BYTES)
        request, safe_id = _parse_request_frame(raw_bytes)
        if request is None:
            reply = _failure(safe_id, REASON_INVALID_REQUEST)
        else:
            reply = apply_review(
                request["data_root"],
                request["workspace_root"],
                request["review_id"],
                request["proposal_digest"],
            )
    except Exception:
        reply = _failure(None, REASON_VERIFICATION_FAILED)

    line = _encode_reply_line(reply)
    try:
        out_stream.write(line)
    except TypeError:
        out_stream.write(line.decode("ascii"))
    if hasattr(out_stream, "flush"):
        out_stream.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
