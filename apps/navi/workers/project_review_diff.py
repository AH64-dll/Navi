# apps/navi/workers/project_review_diff.py
"""Owner-only, read-only helper to preview one validated private review copy.

Stdlib only, no process spawning, no network. Never writes or deletes files.
"""

import difflib
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
from project_review_inventory import (
    FIXED_REVIEW_FILES,
    MAX_REVIEW_FILE_BYTES,
    PROPOSAL_REVIEWS_DIR,
    _open_existing_review_storage,
    inspect_pinned_review_workspace,
)

REQUEST_SCHEMA = "navi-review-preview-request-v1"
REPLY_SCHEMA = "navi-review-preview-v1"

MAX_REQUEST_BYTES = 65536
MAX_REPLY_BYTES = 40000
READ_CHUNK_BYTES = 65536

MAX_DIFF_LINES = 400
MAX_DIFF_CHARS = 24000
DIFF_CONTEXT_LINES = 3
DIFF_FROMFILE = "before"
DIFF_TOFILE = "after"

BEFORE_FILENAME = "before.txt"
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
REASON_INVALID_ENTRY_TYPE = "invalid_entry_type"
REASON_INVALID_PERMISSIONS = "invalid_permissions"
REASON_TOO_LARGE = "too_large"
REASON_INVALID_STORAGE = "invalid_storage"
REASON_CANCELED = "canceled"
REASON_TIMED_OUT = "timed_out"

ALLOWED_REASONS = frozenset(
    {
        REASON_INVALID_REQUEST,
        REASON_NOT_FOUND,
        REASON_DIGEST_MISMATCH,
        REASON_VERIFICATION_FAILED,
        REASON_INVALID_ENTRY_TYPE,
        REASON_INVALID_PERMISSIONS,
        REASON_TOO_LARGE,
        REASON_INVALID_STORAGE,
        REASON_CANCELED,
        REASON_TIMED_OUT,
    }
)

_NON_REGULAR_OPEN_ERRNOS = frozenset(
    code
    for code in (
        errno.ELOOP,
        errno.EISDIR,
        errno.ENOTDIR,
        getattr(errno, "ENXIO", None),
        getattr(errno, "EOPNOTSUPP", None),
    )
    if code is not None
)

_ORIG_OPEN_EXISTING_REVIEW_STORAGE = _open_existing_review_storage
_ORIG_INSPECT_PINNED_REVIEW_WORKSPACE = inspect_pinned_review_workspace


class _PreviewFailure(Exception):
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


def _success(
    review_id,
    *,
    stale,
    truncated,
    diff_lines,
    before_bytes,
    after_bytes,
    diff,
):
    return {
        "schema": REPLY_SCHEMA,
        "review_id": review_id,
        "ok": True,
        "stale": bool(stale),
        "truncated": bool(truncated),
        "diff_lines": diff_lines,
        "before_bytes": before_bytes,
        "after_bytes": after_bytes,
        "diff": diff,
    }


def _failure(review_id, reason):
    safe_reason = reason if reason in ALLOWED_REASONS else REASON_VERIFICATION_FAILED
    return {
        "schema": REPLY_SCHEMA,
        "review_id": _safe_review_id(review_id),
        "ok": False,
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
    raise _PreviewFailure(REASON_INVALID_REQUEST)


def _is_timed_out(deadline):
    return deadline is not None and time.monotonic() >= deadline


def _check_cancel_or_deadline(cancel, deadline):
    if _is_canceled(cancel):
        raise _PreviewFailure(REASON_CANCELED)
    if _is_timed_out(deadline):
        raise _PreviewFailure(REASON_TIMED_OUT)


def _resolve_deadline(timeout_seconds, deadline):
    if deadline is not None:
        if isinstance(deadline, bool) or not isinstance(deadline, (int, float)):
            raise _PreviewFailure(REASON_INVALID_REQUEST)
        if not math.isfinite(deadline):
            raise _PreviewFailure(REASON_INVALID_REQUEST)
        return float(deadline)
    if isinstance(timeout_seconds, bool) or not isinstance(timeout_seconds, (int, float)):
        raise _PreviewFailure(REASON_INVALID_REQUEST)
    if not math.isfinite(timeout_seconds):
        raise _PreviewFailure(REASON_INVALID_REQUEST)
    return time.monotonic() + float(timeout_seconds)


def _map_external_exception(error, *, cancel, deadline, default_reason):
    try:
        if _is_canceled(cancel):
            return REASON_CANCELED
    except _PreviewFailure:
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
        if error.errno in _NON_REGULAR_OPEN_ERRNOS:
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


def _validate_record(record, review_id, proposal_digest):
    if not isinstance(record, dict):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)

    if record.get("valid") is not True:
        raw_reason = record.get("reason")
        if raw_reason in (REASON_CANCELED, REASON_TIMED_OUT):
            raise _PreviewFailure(raw_reason)
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)

    if (
        record.get("status") != "valid"
        or record.get("applied") is not False
        or record.get("review_id") != review_id
    ):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)

    files = record.get("files")
    if (
        not isinstance(files, (list, tuple))
        or len(files) != len(FIXED_REVIEW_FILES)
        or not all(isinstance(name, str) for name in files)
        or set(files) != set(FIXED_REVIEW_FILES)
    ):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)

    record_digest = record.get("proposal_digest")
    if not _is_sha256_hex(record_digest):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)

    original_path = record.get("original_path")
    before_sha256 = record.get("before_sha256")
    after_sha256 = record.get("after_sha256")
    before_bytes = record.get("before_bytes")
    after_bytes = record.get("after_bytes")

    if not _validate_relative_posix_path(original_path):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)
    if not _is_sha256_hex(before_sha256) or not _is_sha256_hex(after_sha256):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)
    if (
        isinstance(before_bytes, bool)
        or not isinstance(before_bytes, int)
        or before_bytes < 0
        or before_bytes > MAX_REVIEW_FILE_BYTES
    ):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)
    if (
        isinstance(after_bytes, bool)
        or not isinstance(after_bytes, int)
        or after_bytes < 0
        or after_bytes > MAX_REVIEW_FILE_BYTES
    ):
        raise _PreviewFailure(REASON_VERIFICATION_FAILED)

    if not hmac.compare_digest(record_digest, proposal_digest):
        raise _PreviewFailure(REASON_DIGEST_MISMATCH)

    return original_path, before_sha256, after_sha256, before_bytes, after_bytes


def _read_pinned_review_file(
    child_fd,
    filename,
    *,
    expected_sha256,
    expected_bytes,
    storage_dev,
    uid,
    cancel,
    deadline,
):
    file_fd = None
    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            file_fd = os.open(
                filename,
                os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=child_fd,
            )
        except FileNotFoundError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error
        except PermissionError as error:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            if error.errno in _NON_REGULAR_OPEN_ERRNOS:
                raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE) from error
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        _check_cancel_or_deadline(cancel, deadline)
        try:
            before_st = os.fstat(file_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISREG(before_st.st_mode):
            raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE)
        if (
            before_st.st_uid != uid
            or before_st.st_dev != storage_dev
            or stat.S_IMODE(before_st.st_mode) != REQUIRED_REVIEW_FILE_MODE
        ):
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS)
        if before_st.st_nlink != 1:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED)
        if before_st.st_size > MAX_REVIEW_FILE_BYTES:
            raise _PreviewFailure(REASON_TOO_LARGE)
        if before_st.st_size != expected_bytes:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED)

        data = bytearray()
        hasher = hashlib.sha256()
        while True:
            _check_cancel_or_deadline(cancel, deadline)
            remaining = MAX_REVIEW_FILE_BYTES + 1 - len(data)
            if remaining <= 0:
                raise _PreviewFailure(REASON_TOO_LARGE)
            to_read = min(READ_CHUNK_BYTES, remaining)
            try:
                chunk = os.read(file_fd, to_read)
            except OSError as error:
                raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error
            _check_cancel_or_deadline(cancel, deadline)
            if not chunk:
                break
            data.extend(chunk)
            hasher.update(chunk)
            if len(data) > MAX_REVIEW_FILE_BYTES:
                raise _PreviewFailure(REASON_TOO_LARGE)

        try:
            after_st = os.fstat(file_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if (
            len(data) != after_st.st_size
            or _file_snapshot(before_st) != _file_snapshot(after_st)
        ):
            raise _PreviewFailure(REASON_VERIFICATION_FAILED)

        if len(data) != expected_bytes:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED)
        if not hmac.compare_digest(hasher.hexdigest(), expected_sha256):
            raise _PreviewFailure(REASON_VERIFICATION_FAILED)

        return bytes(data)
    finally:
        if file_fd is not None:
            fd_to_close, file_fd = file_fd, None
            _safe_close(fd_to_close)


def _inspect_and_read_review(
    data_root,
    review_id,
    proposal_digest,
    *,
    uid,
    cancel,
    deadline,
):
    storage_fd = None
    child_fd = None
    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_fd = _call_open_existing_review_storage(
                data_root, cancel=cancel, deadline=deadline
            )
        except _PreviewFailure:
            raise
        except Exception as error:
            raise _PreviewFailure(
                _map_external_exception(
                    error,
                    cancel=cancel,
                    deadline=deadline,
                    default_reason=REASON_INVALID_STORAGE,
                )
            ) from error

        if storage_fd is None:
            raise _PreviewFailure(REASON_NOT_FOUND)
        if isinstance(storage_fd, bool) or not isinstance(storage_fd, int) or storage_fd < 0:
            storage_fd = None
            raise _PreviewFailure(REASON_INVALID_STORAGE)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_st = os.fstat(storage_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_INVALID_STORAGE) from error

        if not stat.S_ISDIR(storage_st.st_mode):
            raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE)
        if storage_st.st_uid != uid:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            record = _call_inspect_pinned_review_workspace(
                storage_fd,
                review_id,
                storage_dev=storage_st.st_dev,
                cancel=cancel,
                deadline=deadline,
            )
        except _PreviewFailure:
            raise
        except Exception as error:
            raise _PreviewFailure(
                _map_external_exception(
                    error,
                    cancel=cancel,
                    deadline=deadline,
                    default_reason=REASON_VERIFICATION_FAILED,
                )
            ) from error

        _check_cancel_or_deadline(cancel, deadline)
        (
            original_path,
            before_sha256,
            after_sha256,
            before_bytes,
            after_bytes,
        ) = _validate_record(record, review_id, proposal_digest)

        _check_cancel_or_deadline(cancel, deadline)
        dir_flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        if hasattr(os, "O_DIRECTORY"):
            dir_flags |= os.O_DIRECTORY

        try:
            child_fd = os.open(review_id, dir_flags, dir_fd=storage_fd)
        except FileNotFoundError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error
        except PermissionError as error:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            if error.errno in _NON_REGULAR_OPEN_ERRNOS:
                raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE) from error
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        _check_cancel_or_deadline(cancel, deadline)
        try:
            child_st = os.fstat(child_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISDIR(child_st.st_mode):
            raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE)
        if (
            child_st.st_uid != uid
            or child_st.st_dev != storage_st.st_dev
            or stat.S_IMODE(child_st.st_mode) != 0o700
        ):
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS)

        before_data = _read_pinned_review_file(
            child_fd,
            BEFORE_FILENAME,
            expected_sha256=before_sha256,
            expected_bytes=before_bytes,
            storage_dev=storage_st.st_dev,
            uid=uid,
            cancel=cancel,
            deadline=deadline,
        )
        after_data = _read_pinned_review_file(
            child_fd,
            AFTER_FILENAME,
            expected_sha256=after_sha256,
            expected_bytes=after_bytes,
            storage_dev=storage_st.st_dev,
            uid=uid,
            cancel=cancel,
            deadline=deadline,
        )

        _check_cancel_or_deadline(cancel, deadline)
        try:
            after_child_st = os.fstat(child_fd)
            linked_child_st = os.stat(
                review_id, dir_fd=storage_fd, follow_symlinks=False
            )
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if (
            not stat.S_ISDIR(after_child_st.st_mode)
            or after_child_st.st_uid != uid
            or after_child_st.st_dev != storage_st.st_dev
            or stat.S_IMODE(after_child_st.st_mode) != 0o700
            or (linked_child_st.st_dev, linked_child_st.st_ino)
            != (child_st.st_dev, child_st.st_ino)
        ):
            raise _PreviewFailure(REASON_VERIFICATION_FAILED)

        return original_path, before_sha256, before_data, after_data
    finally:
        if child_fd is not None:
            fd_to_close, child_fd = child_fd, None
            _safe_close(fd_to_close)
        if storage_fd is not None:
            fd_to_close, storage_fd = storage_fd, None
            _safe_close(fd_to_close)


def _check_workspace_stale(
    workspace_root,
    original_path,
    *,
    expected_sha256,
    expected_bytes,
    uid,
    cancel,
    deadline,
):
    cur_fd = None
    file_fd = None
    try:
        _check_cancel_or_deadline(cancel, deadline)
        dir_flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        if hasattr(os, "O_DIRECTORY"):
            dir_flags |= os.O_DIRECTORY

        ws_path = workspace_root.rstrip("/") or "/"
        try:
            cur_fd = os.open(ws_path, dir_flags)
        except FileNotFoundError as error:
            raise _PreviewFailure(REASON_NOT_FOUND) from error
        except PermissionError as error:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            if error.errno in _NON_REGULAR_OPEN_ERRNOS:
                raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE) from error
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        _check_cancel_or_deadline(cancel, deadline)
        try:
            ws_st = os.fstat(cur_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISDIR(ws_st.st_mode):
            raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE)
        if ws_st.st_uid != uid:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS)
        ws_dev = ws_st.st_dev

        parts = original_path.split("/")
        for part in parts[:-1]:
            _check_cancel_or_deadline(cancel, deadline)
            try:
                next_fd = os.open(part, dir_flags, dir_fd=cur_fd)
            except FileNotFoundError:
                return True
            except PermissionError as error:
                raise _PreviewFailure(REASON_INVALID_PERMISSIONS) from error
            except OSError as error:
                if error.errno in _NON_REGULAR_OPEN_ERRNOS:
                    raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE) from error
                raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

            old_fd, cur_fd = cur_fd, next_fd
            _safe_close(old_fd)

            _check_cancel_or_deadline(cancel, deadline)
            try:
                dir_st = os.fstat(cur_fd)
            except OSError as error:
                raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

            if not stat.S_ISDIR(dir_st.st_mode):
                raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE)
            if dir_st.st_uid != uid or dir_st.st_dev != ws_dev:
                raise _PreviewFailure(REASON_INVALID_PERMISSIONS)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            file_fd = os.open(
                parts[-1],
                os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=cur_fd,
            )
        except FileNotFoundError:
            return True
        except PermissionError as error:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            if error.errno in _NON_REGULAR_OPEN_ERRNOS:
                raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE) from error
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        _check_cancel_or_deadline(cancel, deadline)
        try:
            before_st = os.fstat(file_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISREG(before_st.st_mode):
            raise _PreviewFailure(REASON_INVALID_ENTRY_TYPE)
        if before_st.st_uid != uid or before_st.st_dev != ws_dev:
            raise _PreviewFailure(REASON_INVALID_PERMISSIONS)
        if before_st.st_size != expected_bytes:
            return True

        total_read = 0
        hasher = hashlib.sha256()
        while True:
            _check_cancel_or_deadline(cancel, deadline)
            remaining = expected_bytes + 1 - total_read
            if remaining <= 0:
                return True
            to_read = min(READ_CHUNK_BYTES, remaining)
            try:
                chunk = os.read(file_fd, to_read)
            except OSError as error:
                raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error
            _check_cancel_or_deadline(cancel, deadline)
            if not chunk:
                break
            total_read += len(chunk)
            hasher.update(chunk)
            if total_read > expected_bytes:
                return True

        try:
            after_st = os.fstat(file_fd)
        except OSError as error:
            raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

        if (
            total_read != after_st.st_size
            or total_read != expected_bytes
            or _file_snapshot(before_st) != _file_snapshot(after_st)
        ):
            return True

        return not hmac.compare_digest(hasher.hexdigest(), expected_sha256)
    finally:
        if file_fd is not None:
            fd_to_close, file_fd = file_fd, None
            _safe_close(fd_to_close)
        if cur_fd is not None:
            fd_to_close, cur_fd = cur_fd, None
            _safe_close(fd_to_close)


def _build_bounded_diff(before_data, after_data, *, cancel, deadline):
    _check_cancel_or_deadline(cancel, deadline)
    try:
        before_text = before_data.decode("utf-8")
        after_text = after_data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise _PreviewFailure(REASON_VERIFICATION_FAILED) from error

    before_lines = before_text.splitlines(keepends=True)
    after_lines = after_text.splitlines(keepends=True)

    kept_lines = []
    total_chars = 0
    truncated = False

    diff_iter = difflib.unified_diff(
        before_lines,
        after_lines,
        fromfile=DIFF_FROMFILE,
        tofile=DIFF_TOFILE,
        n=DIFF_CONTEXT_LINES,
    )

    for raw_line in diff_iter:
        _check_cancel_or_deadline(cancel, deadline)
        line = raw_line[:-1] if raw_line.endswith("\n") else raw_line
        if len(kept_lines) >= MAX_DIFF_LINES:
            truncated = True
            break
        sep_len = 1 if kept_lines else 0
        if total_chars + sep_len + len(line) > MAX_DIFF_CHARS:
            remaining = MAX_DIFF_CHARS - total_chars - sep_len
            if remaining > 0:
                kept_lines.append(line[:remaining])
                total_chars += sep_len + remaining
            truncated = True
            break
        kept_lines.append(line)
        total_chars += sep_len + len(line)

    return "\n".join(kept_lines), len(kept_lines), truncated


def _encode_reply(reply):
    return (
        json.dumps(
            reply,
            separators=(",", ":"),
            ensure_ascii=True,
            allow_nan=False,
        ).encode("utf-8")
        + b"\n"
    )


def _fit_success_reply(
    review_id,
    *,
    stale,
    truncated,
    diff_lines,
    before_bytes,
    after_bytes,
    diff_text,
    cancel,
    deadline,
):
    reply = _success(
        review_id,
        stale=stale,
        truncated=truncated,
        diff_lines=diff_lines,
        before_bytes=before_bytes,
        after_bytes=after_bytes,
        diff=diff_text,
    )
    if len(_encode_reply(reply)) <= MAX_REPLY_BYTES:
        return reply

    best = _success(
        review_id,
        stale=stale,
        truncated=True,
        diff_lines=0,
        before_bytes=before_bytes,
        after_bytes=after_bytes,
        diff="",
    )
    lo = 0
    hi = len(diff_text)
    while lo < hi:
        _check_cancel_or_deadline(cancel, deadline)
        mid = (lo + hi + 1) // 2
        candidate_diff = diff_text[:mid]
        if candidate_diff.endswith("\n"):
            candidate_diff = candidate_diff[:-1]
        candidate_lines = (
            candidate_diff.count("\n") + 1 if candidate_diff else 0
        )
        candidate = _success(
            review_id,
            stale=stale,
            truncated=True,
            diff_lines=candidate_lines,
            before_bytes=before_bytes,
            after_bytes=after_bytes,
            diff=candidate_diff,
        )
        if len(_encode_reply(candidate)) <= MAX_REPLY_BYTES:
            lo = mid
            best = candidate
        else:
            hi = mid - 1

    return best


DEFAULT_TIMEOUT_SECONDS = 10.0


def preview_review_diff(
    data_root,
    workspace_root,
    review_id,
    proposal_digest,
    *,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
    deadline=None,
):
    safe_id = _safe_review_id(review_id)
    resolved_deadline = None
    try:
        resolved_deadline = _resolve_deadline(timeout_seconds, deadline)
        _check_cancel_or_deadline(cancel, resolved_deadline)

        valid_data_root = _validate_root_path(data_root)
        valid_workspace_root = _validate_root_path(workspace_root)
        if (
            valid_data_root is None
            or valid_workspace_root is None
            or safe_id is None
            or not _is_sha256_hex(proposal_digest)
        ):
            raise _PreviewFailure(REASON_INVALID_REQUEST)

        uid = os.geteuid()
        original_path, before_sha256, before_data, after_data = (
            _inspect_and_read_review(
                valid_data_root,
                safe_id,
                proposal_digest,
                uid=uid,
                cancel=cancel,
                deadline=resolved_deadline,
            )
        )
        stale = _check_workspace_stale(
            valid_workspace_root,
            original_path,
            expected_sha256=before_sha256,
            expected_bytes=len(before_data),
            uid=uid,
            cancel=cancel,
            deadline=resolved_deadline,
        )
        diff_text, diff_lines, truncated = _build_bounded_diff(
            before_data,
            after_data,
            cancel=cancel,
            deadline=resolved_deadline,
        )
        reply = _fit_success_reply(
            safe_id,
            stale=stale,
            truncated=truncated,
            diff_lines=diff_lines,
            before_bytes=len(before_data),
            after_bytes=len(after_data),
            diff_text=diff_text,
            cancel=cancel,
            deadline=resolved_deadline,
        )
        _check_cancel_or_deadline(cancel, resolved_deadline)
        return reply
    except _PreviewFailure as error:
        return _failure(safe_id, error.reason)
    except Exception as error:
        return _failure(
            safe_id,
            _map_external_exception(
                error,
                cancel=cancel,
                deadline=resolved_deadline,
                default_reason=REASON_VERIFICATION_FAILED,
            ),
        )


class _RequestError(_PreviewFailure):
    def __init__(self, reason, review_id=None):
        super().__init__(reason)
        self.review_id = _safe_review_id(review_id)


def _strict_object_pairs_hook(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise _RequestError(REASON_INVALID_REQUEST)
        result[key] = value
    return result


def _reject_json_constant(_value):
    raise _RequestError(REASON_INVALID_REQUEST)


def _read_single_request_frame(stdin_stream):
    try:
        raw = stdin_stream.readline(MAX_REQUEST_BYTES + 1)
    except OSError as error:
        raise _RequestError(REASON_INVALID_REQUEST) from error
    if isinstance(raw, str):
        try:
            raw = raw.encode("utf-8")
        except UnicodeEncodeError as error:
            raise _RequestError(REASON_INVALID_REQUEST) from error
    if not raw or len(raw) > MAX_REQUEST_BYTES or not raw.endswith(b"\n"):
        raise _RequestError(REASON_INVALID_REQUEST)
    return raw


def _parse_request_frame(raw_frame):
    if (
        not isinstance(raw_frame, (bytes, bytearray))
        or not raw_frame
        or len(raw_frame) > MAX_REQUEST_BYTES
        or not raw_frame.endswith(b"\n")
    ):
        raise _RequestError(REASON_INVALID_REQUEST)

    body = raw_frame[:-1]
    if b"\n" in body:
        raise _RequestError(REASON_INVALID_REQUEST)

    try:
        text = body.decode("utf-8")
    except UnicodeDecodeError as error:
        raise _RequestError(REASON_INVALID_REQUEST) from error

    try:
        payload = json.loads(
            text,
            object_pairs_hook=_strict_object_pairs_hook,
            parse_constant=_reject_json_constant,
        )
    except _PreviewFailure:
        raise
    except (ValueError, TypeError) as error:
        raise _RequestError(REASON_INVALID_REQUEST) from error

    if not isinstance(payload, dict):
        raise _RequestError(REASON_INVALID_REQUEST)

    safe_id = _safe_review_id(payload.get("review_id"))
    if frozenset(payload.keys()) != EXPECTED_REQUEST_KEYS:
        raise _RequestError(REASON_INVALID_REQUEST, safe_id)
    if payload.get("schema") != REQUEST_SCHEMA:
        raise _RequestError(REASON_INVALID_REQUEST, safe_id)

    data_root = _validate_root_path(payload.get("data_root"))
    workspace_root = _validate_root_path(payload.get("workspace_root"))
    proposal_digest = payload.get("proposal_digest")

    if (
        data_root is None
        or workspace_root is None
        or safe_id is None
        or not _is_sha256_hex(proposal_digest)
    ):
        raise _RequestError(REASON_INVALID_REQUEST, safe_id)

    return data_root, workspace_root, safe_id, proposal_digest


def _write_reply_frame(stdout_stream, reply):
    encoded = _encode_reply(reply)
    if len(encoded) > MAX_REPLY_BYTES:
        encoded = _encode_reply(
            _failure(reply.get("review_id"), REASON_VERIFICATION_FAILED)
        )
    view = memoryview(encoded)
    written = 0
    while written < len(encoded):
        count = stdout_stream.write(view[written:])
        if count is None:
            written = len(encoded)
            break
        if count <= 0:
            raise OSError("short write")
        written += count
    stdout_stream.flush()


def main(argv=None, *, stdin=None, stdout=None):
    args = sys.argv[1:] if argv is None else list(argv)
    stdin_stream = sys.stdin.buffer if stdin is None else stdin
    stdout_stream = sys.stdout.buffer if stdout is None else stdout

    review_id = None
    try:
        if args:
            reply = _failure(None, REASON_INVALID_REQUEST)
        else:
            raw_frame = _read_single_request_frame(stdin_stream)
            data_root, workspace_root, review_id, proposal_digest = (
                _parse_request_frame(raw_frame)
            )
            reply = preview_review_diff(
                data_root,
                workspace_root,
                review_id,
                proposal_digest,
            )
    except _RequestError as error:
        reply = _failure(error.review_id, error.reason)
    except _PreviewFailure as error:
        reply = _failure(review_id, error.reason)
    except Exception:
        reply = _failure(review_id, REASON_VERIFICATION_FAILED)

    try:
        _write_reply_frame(stdout_stream, reply)
    except Exception:
        return 1

    return 0 if reply.get("ok") is True else 1


if __name__ == "__main__":
    raise SystemExit(main())
