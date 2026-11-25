"""Pinned-FD atomic single-file source-write helper (standalone, no route).

Reads one strict JSON request from stdin, walks `<workspace_root>` and each
component of `<relative_path>` strictly via pinned directory file descriptors
(`O_DIRECTORY | O_NOFOLLOW` + `dir_fd`), verifies compare-and-swap against
`expected_before_sha256`, writes decoded bytes to a same-directory temp file
(`O_CREAT | O_EXCL | O_NOFOLLOW`) with the target's existing permission bits
(never wider), fsyncs file and directory, atomically renames via `dir_fd`,
fsyncs the directory again, checks post-rename inode identity, and re-reads
through a fresh fd to verify `new_sha256`.

Never echoes paths or file contents, never touches anything outside the single
target file, and removes its own temp file via `dir_fd` on any pre-rename
failure.
"""
import base64
import binascii
import errno
import hashlib
import hmac
import json
import math
import os
from pathlib import Path
import re
import secrets
import signal
import stat
import sys
import threading
import time

SCHEMA = 'navi-source-write-v1'

REQUEST_KEYS = frozenset(
    {
        'schema',
        'workspace_root',
        'relative_path',
        'expected_before_sha256',
        'new_content_b64',
        'new_sha256',
    }
)

MAX_REQUEST_BYTES = 65536
MAX_FILE_BYTES = 262144
MAX_REL_PATH_BYTES = 512
MAX_REL_PATH_PARTS = 16
MAX_OUTPUT_BYTES = 512

READ_CHUNK_BYTES = 4096
WRITE_CHUNK_BYTES = 4096
HASH_CHUNK_BYTES = 4096
TEMP_CREATE_ATTEMPTS = 8

DEFAULT_TIMEOUT_SECONDS = 10.0
MIN_TIMEOUT_SECONDS = 0.05
MAX_TIMEOUT_SECONDS = 30.0

REASON_INVALID_REQUEST = 'invalid_request'
REASON_NOT_FOUND = 'not_found'
REASON_BEFORE_MISMATCH = 'before_mismatch'
REASON_INVALID_ENTRY_TYPE = 'invalid_entry_type'
REASON_INVALID_PERMISSIONS = 'invalid_permissions'
REASON_TOO_LARGE = 'too_large'
REASON_VERIFICATION_FAILED = 'verification_failed'
REASON_DIRECTORY_CHANGED = 'directory_changed'
REASON_INVALID_STORAGE = 'invalid_storage'

FIXED_REASONS = frozenset(
    {
        REASON_INVALID_REQUEST,
        REASON_NOT_FOUND,
        REASON_BEFORE_MISMATCH,
        REASON_INVALID_ENTRY_TYPE,
        REASON_INVALID_PERMISSIONS,
        REASON_TOO_LARGE,
        REASON_VERIFICATION_FAILED,
        REASON_DIRECTORY_CHANGED,
        REASON_INVALID_STORAGE,
    }
)

_HEX64_RE = re.compile(r'\A[0-9a-f]{64}\Z')
_B64_RE = re.compile(r'\A(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?\Z')


class _WriteFailure(Exception):
    """Controlled failure mapped strictly to a fixed reason code."""

    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


def _failure(reason):
    if reason not in FIXED_REASONS:
        reason = REASON_VERIFICATION_FAILED
    return {
        'schema': SCHEMA,
        'written': False,
        'reason': reason,
    }


def _success(relative_path_digest, before_sha256, after_sha256, byte_count):
    return {
        'schema': SCHEMA,
        'written': True,
        'relative_path_digest': relative_path_digest,
        'before_sha256': before_sha256,
        'after_sha256': after_sha256,
        'bytes': byte_count,
    }


def _check_cancel_or_deadline(cancel, deadline):
    if cancel is not None and cancel.is_set():
        raise _WriteFailure(REASON_VERIFICATION_FAILED)
    if time.monotonic() >= deadline:
        raise _WriteFailure(REASON_VERIFICATION_FAILED)


def _resolve_deadline(timeout_seconds, deadline):
    if deadline is not None:
        if (
            type(deadline) is bool
            or not isinstance(deadline, (int, float))
            or not math.isfinite(deadline)
        ):
            raise _WriteFailure(REASON_INVALID_REQUEST)
        return float(deadline)
    if (
        type(timeout_seconds) is bool
        or not isinstance(timeout_seconds, (int, float))
        or not math.isfinite(timeout_seconds)
        or not (MIN_TIMEOUT_SECONDS <= float(timeout_seconds) <= MAX_TIMEOUT_SECONDS)
    ):
        raise _WriteFailure(REASON_INVALID_REQUEST)
    return time.monotonic() + float(timeout_seconds)


def _strict_json_pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError('Duplicate JSON key')
        result[key] = value
    return result


def _reject_json_constant(_value):
    raise ValueError('Invalid JSON constant')


def _validate_hex64(value):
    if type(value) is not str or _HEX64_RE.fullmatch(value) is None:
        raise _WriteFailure(REASON_INVALID_REQUEST)
    return value


def _validate_workspace_root(workspace_root):
    if type(workspace_root) is not str or not workspace_root or '\x00' in workspace_root:
        raise _WriteFailure(REASON_INVALID_REQUEST)
    if not workspace_root.startswith('/'):
        raise _WriteFailure(REASON_INVALID_REQUEST)
    cleaned = workspace_root.rstrip('/')
    if not cleaned:
        raise _WriteFailure(REASON_INVALID_STORAGE)
    parts = cleaned.split('/')[1:]
    if any(part in ('', '.', '..') for part in parts):
        raise _WriteFailure(REASON_INVALID_REQUEST)
    return cleaned


def _validate_relative_path(relative_path):
    if type(relative_path) is not str or not relative_path or '\x00' in relative_path:
        raise _WriteFailure(REASON_INVALID_REQUEST)
    if relative_path.startswith('/'):
        raise _WriteFailure(REASON_INVALID_REQUEST)
    try:
        encoded = relative_path.encode('utf-8')
    except UnicodeError as error:
        raise _WriteFailure(REASON_INVALID_REQUEST) from error
    if len(encoded) > MAX_REL_PATH_BYTES:
        raise _WriteFailure(REASON_INVALID_REQUEST)
    parts = relative_path.split('/')
    if not (1 <= len(parts) <= MAX_REL_PATH_PARTS):
        raise _WriteFailure(REASON_INVALID_REQUEST)
    for part in parts:
        if part in ('', '.', '..'):
            raise _WriteFailure(REASON_INVALID_REQUEST)
    return parts, encoded


def _decode_content_b64(new_content_b64):
    if type(new_content_b64) is not str or not new_content_b64.isascii():
        raise _WriteFailure(REASON_INVALID_REQUEST)
    if _B64_RE.fullmatch(new_content_b64) is None:
        raise _WriteFailure(REASON_INVALID_REQUEST)
    if not new_content_b64:
        return b''
    padding = 2 if new_content_b64.endswith('==') else (1 if new_content_b64.endswith('=') else 0)
    decoded_len = (len(new_content_b64) // 4) * 3 - padding
    if decoded_len > MAX_FILE_BYTES:
        raise _WriteFailure(REASON_TOO_LARGE)
    try:
        decoded = base64.b64decode(new_content_b64.encode('ascii'), validate=True)
    except (binascii.Error, ValueError) as error:
        raise _WriteFailure(REASON_INVALID_REQUEST) from error
    if len(decoded) > MAX_FILE_BYTES:
        raise _WriteFailure(REASON_TOO_LARGE)
    return decoded


def _sha256_chunked(data, *, cancel, deadline):
    _check_cancel_or_deadline(cancel, deadline)
    hasher = hashlib.sha256()
    view = memoryview(data)
    total = len(view)
    offset = 0
    while offset < total:
        _check_cancel_or_deadline(cancel, deadline)
        end = offset + HASH_CHUNK_BYTES
        hasher.update(view[offset:end])
        offset = end
    _check_cancel_or_deadline(cancel, deadline)
    return hasher.hexdigest()


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
    )


def _read_pinned_fd_chunked(fd, before_st, *, cancel, deadline):
    data = bytearray()
    while True:
        _check_cancel_or_deadline(cancel, deadline)
        remaining = MAX_FILE_BYTES + 1 - len(data)
        if remaining <= 0:
            raise _WriteFailure(REASON_TOO_LARGE)
        to_read = min(READ_CHUNK_BYTES, remaining)
        try:
            chunk = os.read(fd, to_read)
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error
        _check_cancel_or_deadline(cancel, deadline)
        if not chunk:
            break
        data.extend(chunk)
        if len(data) > MAX_FILE_BYTES:
            raise _WriteFailure(REASON_TOO_LARGE)

    try:
        after_st = os.fstat(fd)
    except OSError as error:
        raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

    if len(data) > MAX_FILE_BYTES or after_st.st_size > MAX_FILE_BYTES:
        raise _WriteFailure(REASON_TOO_LARGE)
    if len(data) != after_st.st_size or _file_snapshot(before_st) != _file_snapshot(after_st):
        raise _WriteFailure(REASON_DIRECTORY_CHANGED)
    return bytes(data)


def _open_workspace_root(cleaned_root, *, uid, cancel, deadline):
    _check_cancel_or_deadline(cancel, deadline)
    try:
        root_fd = os.open(
            cleaned_root,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
        )
    except OSError as error:
        raise _WriteFailure(REASON_INVALID_STORAGE) from error

    try:
        _check_cancel_or_deadline(cancel, deadline)
        root_st = os.fstat(root_fd)
        if not stat.S_ISDIR(root_st.st_mode) or root_st.st_uid != uid:
            raise _WriteFailure(REASON_INVALID_STORAGE)
        return root_fd, root_st.st_dev
    except BaseException:
        os.close(root_fd)
        raise


def _open_subdir_pinned(parent_fd, component, *, root_dev, uid, cancel, deadline):
    _check_cancel_or_deadline(cancel, deadline)
    try:
        entry_st = os.lstat(component, dir_fd=parent_fd)
    except FileNotFoundError as error:
        raise _WriteFailure(REASON_NOT_FOUND) from error
    except PermissionError as error:
        raise _WriteFailure(REASON_INVALID_PERMISSIONS) from error
    except OSError as error:
        raise _WriteFailure(REASON_INVALID_STORAGE) from error

    if stat.S_ISLNK(entry_st.st_mode) or not stat.S_ISDIR(entry_st.st_mode):
        raise _WriteFailure(REASON_INVALID_ENTRY_TYPE)
    if entry_st.st_uid != uid or entry_st.st_dev != root_dev:
        raise _WriteFailure(REASON_INVALID_PERMISSIONS)

    _check_cancel_or_deadline(cancel, deadline)
    try:
        child_fd = os.open(
            component,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
    except FileNotFoundError as error:
        raise _WriteFailure(REASON_NOT_FOUND) from error
    except PermissionError as error:
        raise _WriteFailure(REASON_INVALID_PERMISSIONS) from error
    except OSError as error:
        if error.errno in (errno.ELOOP, errno.ENOTDIR):
            raise _WriteFailure(REASON_INVALID_ENTRY_TYPE) from error
        raise _WriteFailure(REASON_INVALID_STORAGE) from error

    try:
        _check_cancel_or_deadline(cancel, deadline)
        child_st = os.fstat(child_fd)
        if not stat.S_ISDIR(child_st.st_mode):
            raise _WriteFailure(REASON_INVALID_ENTRY_TYPE)
        if child_st.st_uid != uid or child_st.st_dev != root_dev:
            raise _WriteFailure(REASON_INVALID_PERMISSIONS)
        if (child_st.st_dev, child_st.st_ino) != (entry_st.st_dev, entry_st.st_ino):
            raise _WriteFailure(REASON_DIRECTORY_CHANGED)
        return child_fd
    except BaseException:
        os.close(child_fd)
        raise


def _read_and_check_target(parent_fd, filename, expected_before_sha256, *, root_dev, uid, cancel, deadline):
    _check_cancel_or_deadline(cancel, deadline)
    try:
        lstat_st = os.lstat(filename, dir_fd=parent_fd)
    except FileNotFoundError as error:
        raise _WriteFailure(REASON_NOT_FOUND) from error
    except PermissionError as error:
        raise _WriteFailure(REASON_INVALID_PERMISSIONS) from error
    except OSError as error:
        raise _WriteFailure(REASON_INVALID_STORAGE) from error

    if stat.S_ISLNK(lstat_st.st_mode) or not stat.S_ISREG(lstat_st.st_mode):
        raise _WriteFailure(REASON_INVALID_ENTRY_TYPE)
    if lstat_st.st_uid != uid or lstat_st.st_nlink != 1 or lstat_st.st_dev != root_dev:
        raise _WriteFailure(REASON_INVALID_PERMISSIONS)
    if lstat_st.st_size > MAX_FILE_BYTES:
        raise _WriteFailure(REASON_TOO_LARGE)

    _check_cancel_or_deadline(cancel, deadline)
    try:
        target_fd = os.open(
            filename,
            os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
    except FileNotFoundError as error:
        raise _WriteFailure(REASON_NOT_FOUND) from error
    except PermissionError as error:
        raise _WriteFailure(REASON_INVALID_PERMISSIONS) from error
    except OSError as error:
        if error.errno in (errno.ELOOP, errno.EISDIR, errno.ENXIO, errno.ENOTDIR):
            raise _WriteFailure(REASON_INVALID_ENTRY_TYPE) from error
        raise _WriteFailure(REASON_INVALID_STORAGE) from error

    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            before_st = os.fstat(target_fd)
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

        if not stat.S_ISREG(before_st.st_mode):
            raise _WriteFailure(REASON_INVALID_ENTRY_TYPE)
        if before_st.st_uid != uid or before_st.st_nlink != 1 or before_st.st_dev != root_dev:
            raise _WriteFailure(REASON_INVALID_PERMISSIONS)
        if (before_st.st_dev, before_st.st_ino) != (lstat_st.st_dev, lstat_st.st_ino):
            raise _WriteFailure(REASON_DIRECTORY_CHANGED)
        if before_st.st_size > MAX_FILE_BYTES:
            raise _WriteFailure(REASON_TOO_LARGE)

        before_bytes = _read_pinned_fd_chunked(
            target_fd, before_st, cancel=cancel, deadline=deadline
        )
    finally:
        os.close(target_fd)

    before_sha256 = _sha256_chunked(before_bytes, cancel=cancel, deadline=deadline)
    if not hmac.compare_digest(before_sha256, expected_before_sha256):
        raise _WriteFailure(REASON_BEFORE_MISMATCH)
    return before_st, before_sha256


def _create_same_dir_temp(parent_fd, target_mode, *, cancel, deadline):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC
    for _ in range(TEMP_CREATE_ATTEMPTS):
        _check_cancel_or_deadline(cancel, deadline)
        candidate = f'.navi-write-{secrets.token_hex(16)}.tmp'
        try:
            fd = os.open(candidate, flags, mode=target_mode, dir_fd=parent_fd)
            return candidate, fd
        except FileExistsError:
            continue
        except PermissionError as error:
            raise _WriteFailure(REASON_INVALID_PERMISSIONS) from error
        except OSError as error:
            raise _WriteFailure(REASON_INVALID_STORAGE) from error
    raise _WriteFailure(REASON_INVALID_STORAGE)


def _write_all_chunked(fd, data, *, cancel, deadline):
    view = memoryview(data)
    total = len(view)
    offset = 0
    while offset < total:
        _check_cancel_or_deadline(cancel, deadline)
        end = offset + WRITE_CHUNK_BYTES
        try:
            written = os.write(fd, view[offset:end])
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error
        if written <= 0:
            raise _WriteFailure(REASON_VERIFICATION_FAILED)
        offset += written
    _check_cancel_or_deadline(cancel, deadline)


def _verify_written_target(parent_fd, filename, temp_st, expected_len, new_sha256, *, root_dev, uid, cancel, deadline):
    _check_cancel_or_deadline(cancel, deadline)
    try:
        final_entry_st = os.stat(filename, dir_fd=parent_fd, follow_symlinks=False)
    except OSError as error:
        raise _WriteFailure(REASON_DIRECTORY_CHANGED) from error

    if (
        stat.S_ISLNK(final_entry_st.st_mode)
        or not stat.S_ISREG(final_entry_st.st_mode)
        or (final_entry_st.st_dev, final_entry_st.st_ino) != (temp_st.st_dev, temp_st.st_ino)
    ):
        raise _WriteFailure(REASON_DIRECTORY_CHANGED)

    _check_cancel_or_deadline(cancel, deadline)
    try:
        verify_fd = os.open(
            filename,
            os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
    except OSError as error:
        if error.errno in (errno.ENOENT, errno.ELOOP, errno.ENOTDIR, errno.EISDIR):
            raise _WriteFailure(REASON_DIRECTORY_CHANGED) from error
        raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

    try:
        _check_cancel_or_deadline(cancel, deadline)
        try:
            verify_st = os.fstat(verify_fd)
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

        if (verify_st.st_dev, verify_st.st_ino) != (temp_st.st_dev, temp_st.st_ino):
            raise _WriteFailure(REASON_DIRECTORY_CHANGED)
        if not stat.S_ISREG(verify_st.st_mode) or verify_st.st_uid != uid or verify_st.st_dev != root_dev:
            raise _WriteFailure(REASON_VERIFICATION_FAILED)
        if verify_st.st_size != expected_len:
            raise _WriteFailure(REASON_VERIFICATION_FAILED)

        verify_bytes = _read_pinned_fd_chunked(
            verify_fd, verify_st, cancel=cancel, deadline=deadline
        )
    finally:
        os.close(verify_fd)

    if len(verify_bytes) != expected_len:
        raise _WriteFailure(REASON_VERIFICATION_FAILED)
    after_sha256 = _sha256_chunked(verify_bytes, cancel=cancel, deadline=deadline)
    if not hmac.compare_digest(after_sha256, new_sha256):
        raise _WriteFailure(REASON_VERIFICATION_FAILED)
    return after_sha256


def _atomic_replace_in_dir(
    parent_fd,
    filename,
    rel_path_bytes,
    expected_before_sha256,
    new_bytes,
    new_sha256,
    *,
    root_dev,
    uid,
    cancel,
    deadline,
):
    before_st, before_sha256 = _read_and_check_target(
        parent_fd,
        filename,
        expected_before_sha256,
        root_dev=root_dev,
        uid=uid,
        cancel=cancel,
        deadline=deadline,
    )
    # Preserve target permission bits; strip setuid/setgid/sticky and never widen.
    target_mode = stat.S_IMODE(before_st.st_mode) & 0o777

    temp_name = None
    temp_fd = None
    renamed = False
    try:
        temp_name, temp_fd = _create_same_dir_temp(
            parent_fd, target_mode, cancel=cancel, deadline=deadline
        )
        _check_cancel_or_deadline(cancel, deadline)
        try:
            os.fchmod(temp_fd, target_mode)
            temp_st = os.fstat(temp_fd)
        except OSError as error:
            raise _WriteFailure(REASON_INVALID_PERMISSIONS) from error

        if (
            not stat.S_ISREG(temp_st.st_mode)
            or temp_st.st_uid != uid
            or temp_st.st_dev != root_dev
            or temp_st.st_nlink != 1
            or stat.S_IMODE(temp_st.st_mode) != target_mode
        ):
            raise _WriteFailure(REASON_INVALID_PERMISSIONS)

        _write_all_chunked(temp_fd, new_bytes, cancel=cancel, deadline=deadline)

        try:
            os.fsync(temp_fd)
            temp_after_st = os.fstat(temp_fd)
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

        if (
            (temp_after_st.st_dev, temp_after_st.st_ino) != (temp_st.st_dev, temp_st.st_ino)
            or temp_after_st.st_size != len(new_bytes)
            or temp_after_st.st_nlink != 1
            or stat.S_IMODE(temp_after_st.st_mode) != target_mode
        ):
            raise _WriteFailure(REASON_DIRECTORY_CHANGED)

        os.close(temp_fd)
        temp_fd = None

        _check_cancel_or_deadline(cancel, deadline)
        try:
            os.fsync(parent_fd)
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

        # Re-check target entry right before rename so concurrent swap aborts cleanly.
        _check_cancel_or_deadline(cancel, deadline)
        try:
            pre_rename_st = os.lstat(filename, dir_fd=parent_fd)
        except OSError as error:
            raise _WriteFailure(REASON_DIRECTORY_CHANGED) from error
        if _file_snapshot(pre_rename_st) != _file_snapshot(before_st):
            raise _WriteFailure(REASON_DIRECTORY_CHANGED)

        _check_cancel_or_deadline(cancel, deadline)
        try:
            os.rename(temp_name, filename, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
            renamed = True
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

        try:
            os.fsync(parent_fd)
        except OSError as error:
            raise _WriteFailure(REASON_VERIFICATION_FAILED) from error

        after_sha256 = _verify_written_target(
            parent_fd,
            filename,
            temp_st,
            len(new_bytes),
            new_sha256,
            root_dev=root_dev,
            uid=uid,
            cancel=cancel,
            deadline=deadline,
        )
        rel_digest = _sha256_chunked(rel_path_bytes, cancel=cancel, deadline=deadline)
        return _success(rel_digest, before_sha256, after_sha256, len(new_bytes))
    finally:
        if temp_fd is not None:
            try:
                os.close(temp_fd)
            except OSError:
                pass
        if temp_name is not None and not renamed:
            try:
                os.unlink(temp_name, dir_fd=parent_fd)
            except OSError:
                pass


def write_source_file(
    workspace_root,
    relative_path,
    expected_before_sha256,
    new_bytes,
    new_sha256,
    *,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
    deadline=None,
):
    """Atomically replace one existing verified file via pinned directory FDs."""
    try:
        effective_deadline = _resolve_deadline(timeout_seconds, deadline)
        _check_cancel_or_deadline(cancel, effective_deadline)

        cleaned_root = _validate_workspace_root(workspace_root)
        parts, rel_path_bytes = _validate_relative_path(relative_path)
        expected_before = _validate_hex64(expected_before_sha256)
        expected_new = _validate_hex64(new_sha256)

        if type(new_bytes) is not bytes:
            raise _WriteFailure(REASON_INVALID_REQUEST)
        if len(new_bytes) > MAX_FILE_BYTES:
            raise _WriteFailure(REASON_TOO_LARGE)

        # Verify new_sha256 against decoded bytes before opening anything.
        computed_new = _sha256_chunked(new_bytes, cancel=cancel, deadline=effective_deadline)
        if not hmac.compare_digest(computed_new, expected_new):
            raise _WriteFailure(REASON_INVALID_REQUEST)

        uid = os.getuid()
        root_fd, root_dev = _open_workspace_root(
            cleaned_root, uid=uid, cancel=cancel, deadline=effective_deadline
        )
        current_fd = root_fd
        try:
            for component in parts[:-1]:
                next_fd = _open_subdir_pinned(
                    current_fd,
                    component,
                    root_dev=root_dev,
                    uid=uid,
                    cancel=cancel,
                    deadline=effective_deadline,
                )
                os.close(current_fd)
                current_fd = next_fd

            return _atomic_replace_in_dir(
                current_fd,
                parts[-1],
                rel_path_bytes,
                expected_before,
                new_bytes,
                expected_new,
                root_dev=root_dev,
                uid=uid,
                cancel=cancel,
                deadline=effective_deadline,
            )
        finally:
            os.close(current_fd)
    except _WriteFailure as error:
        return _failure(error.reason)


def decode_request_bytes(raw_bytes):
    """Strictly parse and validate one stdin JSON request (capped 65536 bytes)."""
    if (
        type(raw_bytes) is not bytes
        or not raw_bytes
        or len(raw_bytes) > MAX_REQUEST_BYTES
        or b'\x00' in raw_bytes
    ):
        raise _WriteFailure(REASON_INVALID_REQUEST)

    try:
        text = raw_bytes.decode('utf-8')
        obj = json.loads(
            text,
            object_pairs_hook=_strict_json_pairs,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeError, ValueError, TypeError, RecursionError) as error:
        raise _WriteFailure(REASON_INVALID_REQUEST) from error

    if type(obj) is not dict or frozenset(obj) != REQUEST_KEYS:
        raise _WriteFailure(REASON_INVALID_REQUEST)
    if obj['schema'] != SCHEMA:
        raise _WriteFailure(REASON_INVALID_REQUEST)

    workspace_root = _validate_workspace_root(obj['workspace_root'])
    _validate_relative_path(obj['relative_path'])
    expected_before_sha256 = _validate_hex64(obj['expected_before_sha256'])
    new_sha256 = _validate_hex64(obj['new_sha256'])
    new_bytes = _decode_content_b64(obj['new_content_b64'])

    return {
        'workspace_root': workspace_root,
        'relative_path': obj['relative_path'],
        'expected_before_sha256': expected_before_sha256,
        'new_bytes': new_bytes,
        'new_sha256': new_sha256,
    }


def process_request_bytes(
    raw_bytes,
    *,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
    deadline=None,
):
    """Decode one strict JSON request and execute the atomic source write."""
    try:
        effective_deadline = _resolve_deadline(timeout_seconds, deadline)
        _check_cancel_or_deadline(cancel, effective_deadline)
        req = decode_request_bytes(raw_bytes)
        return write_source_file(
            req['workspace_root'],
            req['relative_path'],
            req['expected_before_sha256'],
            req['new_bytes'],
            req['new_sha256'],
            cancel=cancel,
            deadline=effective_deadline,
        )
    except _WriteFailure as error:
        return _failure(error.reason)


def serialize_result(result):
    """Validate and serialize a reply to one capped ASCII JSON line."""
    if type(result) is not dict or result.get('schema') != SCHEMA:
        raise ValueError('Invalid source-write result')
    written = result.get('written')
    if type(written) is not bool:
        raise ValueError('Invalid source-write result')

    if written:
        expected_keys = frozenset(
            {
                'schema',
                'written',
                'relative_path_digest',
                'before_sha256',
                'after_sha256',
                'bytes',
            }
        )
        if (
            frozenset(result) != expected_keys
            or type(result['relative_path_digest']) is not str
            or _HEX64_RE.fullmatch(result['relative_path_digest']) is None
            or type(result['before_sha256']) is not str
            or _HEX64_RE.fullmatch(result['before_sha256']) is None
            or type(result['after_sha256']) is not str
            or _HEX64_RE.fullmatch(result['after_sha256']) is None
            or type(result['bytes']) is not int
            or not (0 <= result['bytes'] <= MAX_FILE_BYTES)
        ):
            raise ValueError('Invalid source-write result')
    else:
        if (
            frozenset(result) != frozenset({'schema', 'written', 'reason'})
            or type(result['reason']) is not str
            or result['reason'] not in FIXED_REASONS
        ):
            raise ValueError('Invalid source-write result')

    line = json.dumps(result, ensure_ascii=True, allow_nan=False) + '\n'
    if len(line.encode('ascii')) > MAX_OUTPUT_BYTES:
        raise ValueError('Source-write output exceeded size bound')
    return line


def main(argv=None):
    args = sys.argv[1:] if argv is None else list(argv)
    cancel = threading.Event()
    prev_term = None
    prev_int = None
    installed_signals = False
    if threading.current_thread() is threading.main_thread():
        prev_term = signal.signal(signal.SIGTERM, lambda *_: cancel.set())
        prev_int = signal.signal(signal.SIGINT, lambda *_: cancel.set())
        installed_signals = True
    try:
        if args:
            result = _failure(REASON_INVALID_REQUEST)
        elif cancel.is_set():
            result = _failure(REASON_VERIFICATION_FAILED)
        else:
            raw = sys.stdin.buffer.read(MAX_REQUEST_BYTES + 1)
            result = process_request_bytes(raw, cancel=cancel)
        encoded = serialize_result(result)
        sys.stdout.write(encoded)
        sys.stdout.flush()
        return 0 if result['written'] else 1
    except (OSError, ValueError):
        try:
            sys.stdout.write(serialize_result(_failure(REASON_VERIFICATION_FAILED)))
            sys.stdout.flush()
        except OSError:
            pass
        return 1
    finally:
        if installed_signals:
            signal.signal(signal.SIGTERM, prev_term)
            signal.signal(signal.SIGINT, prev_int)


if __name__ == '__main__':
    raise SystemExit(main())
