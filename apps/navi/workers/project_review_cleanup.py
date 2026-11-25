"""Pinned-FD single-workspace cleanup for owner-private inert review workspaces.

Deletes at most ONE verified `review-<32hex>` directory inside
`<owner_data>/proposal-reviews` after re-verifying the workspace through
`project_review_inventory.inspect_pinned_review_workspace` and binding deletion
to a caller-supplied expected `proposal_digest`.

All inspection and deletion runs solely through pinned directory file
descriptors (`dir_fd` + `O_NOFOLLOW | O_DIRECTORY`), bounded `os.scandir`,
inode-identity verification before unlinking and before `rmdir`, and
cancel/deadline checks at each step. Never touches project files, never echoes
paths or contents, and is not wired to the daemon or UI.
"""
import errno
import hmac
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import sys
import threading
import time

_WORKERS_DIR = str(Path(__file__).resolve().parent)
if _WORKERS_DIR not in sys.path:
    sys.path.insert(0, _WORKERS_DIR)

from project_read import ProjectReadError
from project_review_inventory import (
    DEFAULT_TIMEOUT_SECONDS,
    FIXED_REVIEW_FILES,
    FIXED_REVIEW_FILE_SET,
    MAX_REQUEST_FRAME_BYTES,
    MAX_TIMEOUT_SECONDS,
    MIN_TIMEOUT_SECONDS,
    PROPOSAL_REVIEWS_DIR,
    _InventoryAborted,
    inspect_pinned_review_workspace,
)
from project_workspace import _storage

CLEANUP_SCHEMA = 'navi-review-cleanup-v1'
REQUEST_SCHEMA = 'navi-review-cleanup-request-v1'
REQUEST_KEYS = frozenset({'schema', 'review_id', 'proposal_digest'})

MAX_OUTPUT_BYTES = 512

REASON_NOT_FOUND = 'not_found'
REASON_DIGEST_MISMATCH = 'digest_mismatch'
REASON_INVALID_ENTRY_TYPE = 'invalid_entry_type'
REASON_INVALID_PERMISSIONS = 'invalid_permissions'
REASON_INCOMPLETE_OR_EXTRA_FILES = 'incomplete_or_extra_files'
REASON_VERIFICATION_FAILED = 'verification_failed'
REASON_DIRECTORY_CHANGED = 'directory_changed'
REASON_CANCELED = 'canceled'
REASON_TIMED_OUT = 'timed_out'
REASON_INVALID_REQUEST = 'invalid_request'
REASON_INVALID_STORAGE = 'invalid_storage'

FIXED_REASONS = frozenset(
    {
        REASON_NOT_FOUND,
        REASON_DIGEST_MISMATCH,
        REASON_INVALID_ENTRY_TYPE,
        REASON_INVALID_PERMISSIONS,
        REASON_INCOMPLETE_OR_EXTRA_FILES,
        REASON_VERIFICATION_FAILED,
        REASON_DIRECTORY_CHANGED,
        REASON_CANCELED,
        REASON_TIMED_OUT,
        REASON_INVALID_REQUEST,
        REASON_INVALID_STORAGE,
    }
)

_INSPECT_INVALID_REASONS = frozenset(
    {
        REASON_INVALID_ENTRY_TYPE,
        REASON_INVALID_PERMISSIONS,
        REASON_INCOMPLETE_OR_EXTRA_FILES,
        REASON_VERIFICATION_FAILED,
    }
)

_NULL_REVIEW_ID_REASONS = frozenset(
    {
        REASON_INVALID_REQUEST,
        REASON_INVALID_STORAGE,
        REASON_CANCELED,
        REASON_TIMED_OUT,
    }
)

_REVIEW_ID_RE = re.compile(r'\Areview-[0-9a-f]{32}\Z')
_HEX64_RE = re.compile(r'\A[0-9a-f]{64}\Z')


class _CleanupAborted(ProjectReadError):
    """Cancellation or deadline expiry during private review cleanup."""

    def __init__(self, reason, message):
        super().__init__(message)
        self.reason = reason


class _CleanupStorageError(ProjectReadError):
    """Owner data root or proposal-reviews storage failed verification."""


def _check_cancel_or_deadline(cancel, deadline):
    if cancel is not None and cancel.is_set():
        raise _CleanupAborted(REASON_CANCELED, 'Private review cleanup canceled')
    if time.monotonic() >= deadline:
        raise _CleanupAborted(REASON_TIMED_OUT, 'Private review cleanup timed out')


def _abort_reason(cancel, error=None):
    if cancel is not None and cancel.is_set():
        return REASON_CANCELED
    if isinstance(error, _CleanupAborted) and error.reason in (
        REASON_CANCELED,
        REASON_TIMED_OUT,
    ):
        return error.reason
    return REASON_TIMED_OUT


def _validate_timeout(timeout_seconds):
    if (
        type(timeout_seconds) is bool
        or not isinstance(timeout_seconds, (int, float))
        or not math.isfinite(timeout_seconds)
        or not (MIN_TIMEOUT_SECONDS <= float(timeout_seconds) <= MAX_TIMEOUT_SECONDS)
    ):
        raise ProjectReadError('Invalid cleanup timeout')
    return float(timeout_seconds)


def _validate_review_id(review_id):
    if type(review_id) is not str or _REVIEW_ID_RE.fullmatch(review_id) is None:
        raise ProjectReadError('Invalid private review identifier')
    return review_id


def _validate_proposal_digest(expected_digest):
    if type(expected_digest) is not str or _HEX64_RE.fullmatch(expected_digest) is None:
        raise ProjectReadError('Invalid proposal digest')
    return expected_digest


def _strict_json_pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError('Duplicate JSON key')
        result[key] = value
    return result


def _reject_json_constant(_value):
    raise ValueError('Invalid JSON constant')


def _result(deleted, review_id, reason=None):
    if deleted:
        if reason is not None:
            raise ProjectReadError('Deleted cleanup result must not include a reason')
        return {
            'schema': CLEANUP_SCHEMA,
            'deleted': True,
            'review_id': review_id,
        }
    if reason not in FIXED_REASONS:
        raise ProjectReadError('Invalid cleanup reason code')
    return {
        'schema': CLEANUP_SCHEMA,
        'deleted': False,
        'review_id': review_id,
        'reason': reason,
    }


def _dir_full_snapshot(st):
    return (
        st.st_dev,
        st.st_ino,
        st.st_uid,
        st.st_mode,
        st.st_nlink,
        st.st_mtime_ns,
        st.st_ctime_ns,
    )


def _file_full_snapshot(st):
    return (
        st.st_dev,
        st.st_ino,
        st.st_size,
        st.st_mtime_ns,
        st.st_ctime_ns,
        st.st_nlink,
        st.st_mode,
        st.st_uid,
    )


def _is_valid_dir_stat(st, *, storage_dev, uid):
    return (
        not stat.S_ISLNK(st.st_mode)
        and stat.S_ISDIR(st.st_mode)
        and st.st_dev == storage_dev
        and st.st_uid == uid
        and stat.S_IMODE(st.st_mode) == 0o700
    )


def _is_valid_file_stat(st, *, storage_dev, uid):
    return (
        not stat.S_ISLNK(st.st_mode)
        and stat.S_ISREG(st.st_mode)
        and st.st_dev == storage_dev
        and st.st_uid == uid
        and st.st_nlink == 1
        and stat.S_IMODE(st.st_mode) == 0o600
    )


def _open_existing_review_storage(data_root, *, cancel, deadline):
    """Open `<data_root>/proposal-reviews` via pinned FDs without creating dirs."""
    _check_cancel_or_deadline(cancel, deadline)
    if not isinstance(data_root, (str, os.PathLike)):
        raise _CleanupStorageError('Owner data root must be an absolute path')
    try:
        raw_root = os.fspath(data_root)
    except TypeError as error:
        raise _CleanupStorageError('Owner data root must be an absolute path') from error
    if type(raw_root) is not str or not raw_root or '\x00' in raw_root:
        raise _CleanupStorageError('Owner data root must be an absolute path')
    root_path = Path(raw_root)
    if not root_path.is_absolute() or '..' in root_path.parts or len(root_path.parts) <= 1:
        raise _CleanupStorageError('Owner data root must be an absolute non-root path')

    try:
        parent_fd = _storage(root_path)
    except (_CleanupAborted, _InventoryAborted):
        raise
    except (ProjectReadError, OSError, ValueError) as error:
        raise _CleanupStorageError(
            'Owner data root is not a verified owner-only directory'
        ) from error

    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            storage_fd = os.open(
                PROPOSAL_REVIEWS_DIR,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=parent_fd,
            )
        except FileNotFoundError:
            return None
        except OSError as error:
            raise _CleanupStorageError(
                'Private review storage is not a verified owner-only directory'
            ) from error
        try:
            _check_cancel_or_deadline(cancel, deadline)
            info = os.fstat(storage_fd)
            if (
                not stat.S_ISDIR(info.st_mode)
                or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o700
            ):
                raise _CleanupStorageError('Private review storage is not owner-private')
            return storage_fd
        except BaseException:
            os.close(storage_fd)
            raise
    finally:
        os.close(parent_fd)


def _bounded_child_names(dir_fd, max_entries, *, cancel, deadline):
    """Stream at most max_entries names via scandir(fd); return None on overflow/error."""
    _check_cancel_or_deadline(cancel, deadline)
    names = []
    try:
        with os.scandir(dir_fd) as entries:
            for entry in entries:
                _check_cancel_or_deadline(cancel, deadline)
                if len(names) >= max_entries:
                    return None
                names.append(entry.name)
    except OSError:
        return None
    _check_cancel_or_deadline(cancel, deadline)
    return names


def _snapshot_fixed_files(child_fd, *, storage_dev, uid, cancel, deadline):
    """Snapshot the three fixed regular files via pinned child_fd, or return None."""
    names = _bounded_child_names(
        child_fd, len(FIXED_REVIEW_FILES), cancel=cancel, deadline=deadline
    )
    if names is None or frozenset(names) != FIXED_REVIEW_FILE_SET:
        return None
    snapshots = {}
    for filename in FIXED_REVIEW_FILES:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            file_st = os.lstat(filename, dir_fd=child_fd)
        except OSError:
            return None
        if not _is_valid_file_stat(file_st, storage_dev=storage_dev, uid=uid):
            return None
        snapshots[filename] = _file_full_snapshot(file_st)
    return snapshots


def _verify_pinned_dir_identity(
    storage_fd,
    child_fd,
    review_id,
    dir_before,
    *,
    storage_dev,
    uid,
    require_unchanged_times,
):
    """Verify storage_fd, storage entry, and child_fd match the pinned dir inode."""
    try:
        storage_st = os.fstat(storage_fd)
        entry_st = os.lstat(review_id, dir_fd=storage_fd)
        child_st = os.fstat(child_fd)
    except OSError:
        return False

    if not _is_valid_dir_stat(storage_st, storage_dev=storage_dev, uid=uid):
        return False
    if not _is_valid_dir_stat(entry_st, storage_dev=storage_dev, uid=uid):
        return False
    if not _is_valid_dir_stat(child_st, storage_dev=storage_dev, uid=uid):
        return False

    expected_id = (dir_before.st_dev, dir_before.st_ino)
    if (entry_st.st_dev, entry_st.st_ino) != expected_id:
        return False
    if (child_st.st_dev, child_st.st_ino) != expected_id:
        return False
    if child_st.st_nlink != dir_before.st_nlink or entry_st.st_nlink != dir_before.st_nlink:
        return False
    if require_unchanged_times:
        expected_snapshot = _dir_full_snapshot(dir_before)
        if _dir_full_snapshot(child_st) != expected_snapshot:
            return False
        if _dir_full_snapshot(entry_st) != expected_snapshot:
            return False
    return True


def cleanup_pinned_review_workspace(
    storage_fd,
    review_id,
    expected_digest,
    *,
    storage_dev=None,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
    deadline=None,
):
    """Verify and delete one review workspace solely via pinned directory FDs.

    Never reopens by path, never deletes outside the single verified review
    directory, binds deletion to `expected_digest`, and stops immediately on
    any mid-way inode/metadata mutation, cancellation, or deadline expiry.
    """
    if type(storage_fd) is not int or storage_fd < 0:
        raise _CleanupStorageError('Invalid private review storage descriptor')
    validated_review_id = _validate_review_id(review_id)
    validated_digest = _validate_proposal_digest(expected_digest)

    if deadline is None:
        timeout = _validate_timeout(timeout_seconds)
        effective_deadline = time.monotonic() + timeout
    else:
        if (
            type(deadline) is bool
            or not isinstance(deadline, (int, float))
            or not math.isfinite(deadline)
        ):
            raise ProjectReadError('Invalid cleanup deadline')
        effective_deadline = float(deadline)

    uid = os.getuid()
    try:
        storage_st = os.fstat(storage_fd)
    except OSError as error:
        raise _CleanupStorageError('Private review storage descriptor is unavailable') from error
    if (
        not stat.S_ISDIR(storage_st.st_mode)
        or storage_st.st_uid != uid
        or stat.S_IMODE(storage_st.st_mode) != 0o700
    ):
        raise _CleanupStorageError('Private review storage is not owner-private')
    if storage_dev is not None and storage_st.st_dev != storage_dev:
        raise _CleanupStorageError('Private review storage device mismatch')
    resolved_storage_dev = storage_st.st_dev

    try:
        _check_cancel_or_deadline(cancel, effective_deadline)
        try:
            top_st = os.lstat(validated_review_id, dir_fd=storage_fd)
        except FileNotFoundError:
            return _result(False, validated_review_id, REASON_NOT_FOUND)
        except OSError:
            return _result(False, validated_review_id, REASON_VERIFICATION_FAILED)

        if stat.S_ISLNK(top_st.st_mode) or not stat.S_ISDIR(top_st.st_mode):
            return _result(False, validated_review_id, REASON_INVALID_ENTRY_TYPE)
        if (
            top_st.st_dev != resolved_storage_dev
            or top_st.st_uid != uid
            or stat.S_IMODE(top_st.st_mode) != 0o700
        ):
            return _result(False, validated_review_id, REASON_INVALID_PERMISSIONS)

        _check_cancel_or_deadline(cancel, effective_deadline)
        try:
            child_fd = os.open(
                validated_review_id,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=storage_fd,
            )
        except FileNotFoundError:
            return _result(False, validated_review_id, REASON_NOT_FOUND)
        except PermissionError:
            return _result(False, validated_review_id, REASON_INVALID_PERMISSIONS)
        except OSError as error:
            if error.errno in (errno.ELOOP, errno.ENOTDIR):
                return _result(False, validated_review_id, REASON_INVALID_ENTRY_TYPE)
            return _result(False, validated_review_id, REASON_VERIFICATION_FAILED)

        try:
            _check_cancel_or_deadline(cancel, effective_deadline)
            try:
                dir_before = os.fstat(child_fd)
            except OSError:
                return _result(False, validated_review_id, REASON_VERIFICATION_FAILED)

            if not stat.S_ISDIR(dir_before.st_mode):
                return _result(False, validated_review_id, REASON_INVALID_ENTRY_TYPE)
            if (
                dir_before.st_dev != resolved_storage_dev
                or dir_before.st_uid != uid
                or stat.S_IMODE(dir_before.st_mode) != 0o700
            ):
                return _result(False, validated_review_id, REASON_INVALID_PERMISSIONS)
            if (dir_before.st_dev, dir_before.st_ino) != (top_st.st_dev, top_st.st_ino):
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            files_before = _snapshot_fixed_files(
                child_fd,
                storage_dev=resolved_storage_dev,
                uid=uid,
                cancel=cancel,
                deadline=effective_deadline,
            )

            _check_cancel_or_deadline(cancel, effective_deadline)
            try:
                record = inspect_pinned_review_workspace(
                    storage_fd,
                    validated_review_id,
                    storage_dev=resolved_storage_dev,
                    cancel=cancel,
                    deadline=effective_deadline,
                )
            except _InventoryAborted:
                raise
            except ProjectReadError as error:
                raise _CleanupStorageError(
                    'Private review storage is unavailable'
                ) from error
            _check_cancel_or_deadline(cancel, effective_deadline)

            if type(record) is not dict:
                return _result(False, validated_review_id, REASON_VERIFICATION_FAILED)

            if record.get('valid') is not True:
                if files_before is not None and not _verify_pinned_dir_identity(
                    storage_fd,
                    child_fd,
                    validated_review_id,
                    dir_before,
                    storage_dev=resolved_storage_dev,
                    uid=uid,
                    require_unchanged_times=True,
                ):
                    return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)
                raw_reason = record.get('reason')
                reason = (
                    raw_reason
                    if raw_reason in _INSPECT_INVALID_REASONS
                    else REASON_VERIFICATION_FAILED
                )
                return _result(False, validated_review_id, reason)

            record_digest = record.get('proposal_digest')
            record_files = record.get('files')
            if (
                record.get('review_id') != validated_review_id
                or record.get('status') != 'valid'
                or record.get('applied') is not False
                or type(record_digest) is not str
                or _HEX64_RE.fullmatch(record_digest) is None
                or type(record_files) is not list
                or tuple(record_files) != FIXED_REVIEW_FILES
            ):
                return _result(False, validated_review_id, REASON_VERIFICATION_FAILED)

            if not hmac.compare_digest(record_digest, validated_digest):
                return _result(False, validated_review_id, REASON_DIGEST_MISMATCH)

            if files_before is None:
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            # Verify directory inode identity and all three files before starting unlink.
            _check_cancel_or_deadline(cancel, effective_deadline)
            if not _verify_pinned_dir_identity(
                storage_fd,
                child_fd,
                validated_review_id,
                dir_before,
                storage_dev=resolved_storage_dev,
                uid=uid,
                require_unchanged_times=True,
            ):
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            files_now = _snapshot_fixed_files(
                child_fd,
                storage_dev=resolved_storage_dev,
                uid=uid,
                cancel=cancel,
                deadline=effective_deadline,
            )
            if files_now is None or files_now != files_before:
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            # Delete only the three fixed files via unlinkat(child_fd, name, 0).
            for index, filename in enumerate(FIXED_REVIEW_FILES):
                _check_cancel_or_deadline(cancel, effective_deadline)
                if not _verify_pinned_dir_identity(
                    storage_fd,
                    child_fd,
                    validated_review_id,
                    dir_before,
                    storage_dev=resolved_storage_dev,
                    uid=uid,
                    require_unchanged_times=(index == 0),
                ):
                    return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

                try:
                    current_file_st = os.lstat(filename, dir_fd=child_fd)
                except OSError:
                    return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)
                if (
                    not _is_valid_file_stat(
                        current_file_st, storage_dev=resolved_storage_dev, uid=uid
                    )
                    or _file_full_snapshot(current_file_st) != files_before[filename]
                ):
                    return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

                _check_cancel_or_deadline(cancel, effective_deadline)
                try:
                    os.unlink(filename, dir_fd=child_fd)
                except OSError:
                    return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            # Verify directory inode identity and emptiness again before rmdir.
            _check_cancel_or_deadline(cancel, effective_deadline)
            if not _verify_pinned_dir_identity(
                storage_fd,
                child_fd,
                validated_review_id,
                dir_before,
                storage_dev=resolved_storage_dev,
                uid=uid,
                require_unchanged_times=False,
            ):
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            remaining_names = _bounded_child_names(
                child_fd, 0, cancel=cancel, deadline=effective_deadline
            )
            if remaining_names is None or remaining_names:
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            _check_cancel_or_deadline(cancel, effective_deadline)
            try:
                os.rmdir(validated_review_id, dir_fd=storage_fd)
            except OSError:
                return _result(False, validated_review_id, REASON_DIRECTORY_CHANGED)

            return _result(True, validated_review_id)
        finally:
            os.close(child_fd)
    except (_CleanupAborted, _InventoryAborted) as error:
        return _result(False, validated_review_id, _abort_reason(cancel, error))


def cleanup_review_workspace(
    data_root,
    review_id,
    expected_digest,
    *,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
):
    """Delete one verified review workspace inside `<data_root>/proposal-reviews`."""
    validated_review_id = _validate_review_id(review_id)
    validated_digest = _validate_proposal_digest(expected_digest)
    timeout = _validate_timeout(timeout_seconds)
    deadline = time.monotonic() + timeout

    try:
        _check_cancel_or_deadline(cancel, deadline)
        storage_fd = _open_existing_review_storage(
            data_root, cancel=cancel, deadline=deadline
        )
    except (_CleanupAborted, _InventoryAborted) as error:
        return _result(False, validated_review_id, _abort_reason(cancel, error))

    if storage_fd is None:
        return _result(False, validated_review_id, REASON_NOT_FOUND)

    try:
        storage_dev = os.fstat(storage_fd).st_dev
        return cleanup_pinned_review_workspace(
            storage_fd,
            validated_review_id,
            validated_digest,
            storage_dev=storage_dev,
            cancel=cancel,
            deadline=deadline,
        )
    except (_CleanupAborted, _InventoryAborted) as error:
        return _result(False, validated_review_id, _abort_reason(cancel, error))
    finally:
        os.close(storage_fd)


def serialize_cleanup_result(result, *, max_bytes=None):
    """Serialize a cleanup response to one strictly validated ASCII JSON line."""
    cap = MAX_OUTPUT_BYTES if max_bytes is None else max_bytes
    if type(cap) is not int or cap <= 0:
        raise ProjectReadError('Invalid cleanup output size bound')
    if type(result) is not dict:
        raise ProjectReadError('Invalid cleanup output payload')

    schema = result.get('schema')
    deleted = result.get('deleted')
    review_id = result.get('review_id')
    if schema != CLEANUP_SCHEMA or type(deleted) is not bool:
        raise ProjectReadError('Invalid cleanup output payload')

    if deleted:
        if (
            frozenset(result) != frozenset({'schema', 'deleted', 'review_id'})
            or type(review_id) is not str
            or _REVIEW_ID_RE.fullmatch(review_id) is None
        ):
            raise ProjectReadError('Invalid cleanup output payload')
    else:
        reason = result.get('reason')
        if (
            frozenset(result) != frozenset({'schema', 'deleted', 'review_id', 'reason'})
            or type(reason) is not str
            or reason not in FIXED_REASONS
        ):
            raise ProjectReadError('Invalid cleanup output payload')
        if review_id is None:
            if reason not in _NULL_REVIEW_ID_REASONS:
                raise ProjectReadError('Invalid cleanup output payload')
        elif type(review_id) is not str or _REVIEW_ID_RE.fullmatch(review_id) is None:
            raise ProjectReadError('Invalid cleanup output payload')

    try:
        line = json.dumps(result, ensure_ascii=True, allow_nan=False) + '\n'
    except (TypeError, ValueError) as error:
        raise ProjectReadError('Invalid cleanup output payload') from error
    if len(line.encode('ascii')) > cap:
        raise ProjectReadError('Private review cleanup output exceeded size bound')
    return line


def decode_cleanup_frame(frame):
    """Decode one fixed cleanup request frame. Rejects paths, commands, or extra keys."""
    if (
        type(frame) is not bytes
        or not frame
        or len(frame) > MAX_REQUEST_FRAME_BYTES
        or not frame.endswith(b'\n')
        or frame.find(b'\n') != len(frame) - 1
        or b'\r' in frame
        or b'\x00' in frame
    ):
        raise ProjectReadError('Invalid review cleanup request frame')

    try:
        text = frame[:-1].decode('ascii')
        value = json.loads(
            text,
            object_pairs_hook=_strict_json_pairs,
            parse_constant=_reject_json_constant,
        )
        if (
            type(value) is not dict
            or frozenset(value) != REQUEST_KEYS
            or type(value['schema']) is not str
            or value['schema'] != REQUEST_SCHEMA
            or type(value['review_id']) is not str
            or _REVIEW_ID_RE.fullmatch(value['review_id']) is None
            or type(value['proposal_digest']) is not str
            or _HEX64_RE.fullmatch(value['proposal_digest']) is None
        ):
            raise ValueError()
    except (ValueError, TypeError, UnicodeError, RecursionError) as error:
        raise ProjectReadError('Invalid review cleanup request') from error
    return value


def cleanup_request(data_root, frame, *, cancel=None, timeout_seconds=DEFAULT_TIMEOUT_SECONDS):
    request = decode_cleanup_frame(frame)
    return cleanup_review_workspace(
        data_root,
        request['review_id'],
        request['proposal_digest'],
        cancel=cancel,
        timeout_seconds=timeout_seconds,
    )


def _emit_result(result):
    encoded = serialize_cleanup_result(result)
    try:
        sys.stdout.write(encoded)
        sys.stdout.flush()
    except OSError:
        pass


def main():
    cancel = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: cancel.set())
    signal.signal(signal.SIGINT, lambda *_: cancel.set())
    validated_review_id = None
    try:
        if len(sys.argv) != 3 or sys.argv[1] != '--owner-data' or not sys.argv[2]:
            raise ProjectReadError('Fixed --owner-data argument required')
        if cancel.is_set():
            _emit_result(_result(False, None, REASON_CANCELED))
            return 1
        frame = sys.stdin.buffer.readline(MAX_REQUEST_FRAME_BYTES + 1)
        if cancel.is_set():
            _emit_result(_result(False, None, REASON_CANCELED))
            return 1
        request = decode_cleanup_frame(frame)
        validated_review_id = request['review_id']
        result = cleanup_review_workspace(
            sys.argv[2],
            validated_review_id,
            request['proposal_digest'],
            cancel=cancel,
        )
        _emit_result(result)
        if result['deleted'] or result.get('reason') == REASON_NOT_FOUND:
            return 0
        return 1
    except _CleanupStorageError:
        _emit_result(_result(False, validated_review_id, REASON_INVALID_STORAGE))
        return 1
    except (_CleanupAborted, _InventoryAborted) as error:
        _emit_result(_result(False, validated_review_id, _abort_reason(cancel, error)))
        return 1
    except (ProjectReadError, OSError):
        _emit_result(_result(False, None, REASON_INVALID_REQUEST))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
