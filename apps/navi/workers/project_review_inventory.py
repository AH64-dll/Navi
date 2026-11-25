"""Read-only metadata inventory for owner-private inert review workspaces.

Inspects ONLY the fixed `<owner_data>/proposal-reviews` directory without
writing, creating directories, deleting, cleaning up, or executing anything.
All directory and file inspection runs through pinned directory file descriptors
(`dir_fd` + `O_NOFOLLOW`) with bounded `os.scandir`, chunked reads/hashing with
in-loop cancel/deadline checks, and a capped serialized output size.

No arbitrary root/path or command is accepted from stdin JSON. Public candidate
execution remains unavailable and no daemon/UI wiring is included here.
"""
import errno
import hashlib
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
from project_edit_reviews import proposal_digest
from project_edits import propose_edit
from project_read import ProjectReadError
from project_workspace import _storage

INVENTORY_SCHEMA = 'navi-review-inventory-v1'
REQUEST_SCHEMA = 'navi-review-inventory-request-v1'
REVIEW_MANIFEST_SCHEMA = 'navi-private-review-v1'
PROPOSAL_REVIEWS_DIR = 'proposal-reviews'
FIXED_REVIEW_FILES = ('before.txt', 'proposed-after.txt', 'review.json')
FIXED_REVIEW_FILE_SET = frozenset(FIXED_REVIEW_FILES)
MANIFEST_KEYS = frozenset(
    {
        'schema',
        'applied',
        'requires_fresh_validation',
        'original_path',
        'proposal_digest',
        'before_sha256',
        'after_sha256',
        'notice',
    }
)

MAX_INSPECTED_CHILDREN = 64
MAX_VALID_RECORDS = 32
MAX_REVIEW_FILE_BYTES = 16384
READ_CHUNK_BYTES = 1024
HASH_CHUNK_BYTES = 1024
MAX_REQUEST_FRAME_BYTES = 256
MAX_OUTPUT_BYTES = 65536

DEFAULT_TIMEOUT_SECONDS = 4.0
MIN_TIMEOUT_SECONDS = 0.05
MAX_TIMEOUT_SECONDS = 10.0

_REVIEW_ID_RE = re.compile(r'^review-[0-9a-f]{32}$')
_HEX64_RE = re.compile(r'^[0-9a-f]{64}$')
_UNSAFE_TEXT_RE = re.compile(r'[\x00-\x1f\x7f\u202a-\u202e\u2066-\u2069]')

_INVENTORY_NOTICE = (
    'Metadata inventory of private inert review workspaces only. '
    'No source contents are returned, no files were modified or deleted, '
    'and no review has been applied or revalidated against live project files.'
)


class _InventoryAborted(ProjectReadError):
    """Cancellation or deadline expiry; never downgraded to an invalid record."""


def _check_cancel_or_deadline(cancel, deadline):
    if cancel is not None and cancel.is_set():
        raise _InventoryAborted('Private review inventory canceled')
    if time.monotonic() >= deadline:
        raise _InventoryAborted('Private review inventory timed out')


def _validate_timeout(timeout_seconds):
    if (
        type(timeout_seconds) is bool
        or not isinstance(timeout_seconds, (int, float))
        or not math.isfinite(timeout_seconds)
        or not (MIN_TIMEOUT_SECONDS <= float(timeout_seconds) <= MAX_TIMEOUT_SECONDS)
    ):
        raise ProjectReadError('Invalid inventory timeout')
    return float(timeout_seconds)


def _strict_json_pairs(items):
    result = {}
    for key, value in items:
        if key in result:
            raise ValueError('Duplicate JSON key')
        result[key] = value
    return result


def _reject_json_constant(_value):
    raise ValueError('Invalid JSON constant')


def _open_existing_review_storage(data_root, *, cancel, deadline):
    """Open `<data_root>/proposal-reviews` strictly read-only. Never calls mkdir."""
    _check_cancel_or_deadline(cancel, deadline)
    if not isinstance(data_root, (str, os.PathLike)):
        raise ProjectReadError('Owner data root must be an absolute path')
    root_path = Path(data_root)
    if not root_path.is_absolute() or '..' in root_path.parts or len(root_path.parts) <= 1:
        raise ProjectReadError('Owner data root must be an absolute non-root path')

    try:
        parent_fd = _storage(root_path)
    except OSError as error:
        raise ProjectReadError(
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
            raise ProjectReadError(
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
                raise ProjectReadError('Private review storage is not owner-private')
            return storage_fd
        except BaseException:
            os.close(storage_fd)
            raise
    finally:
        os.close(parent_fd)


def _bounded_scandir_names(dir_fd, max_entries, *, cancel, deadline):
    """Stream at most max_entries names via scandir(fd); refuse overflow."""
    _check_cancel_or_deadline(cancel, deadline)
    names = []
    with os.scandir(dir_fd) as entries:
        for entry in entries:
            _check_cancel_or_deadline(cancel, deadline)
            if len(names) >= max_entries:
                raise ProjectReadError('Directory entry count exceeded inventory bound')
            names.append(entry.name)
    _check_cancel_or_deadline(cancel, deadline)
    names.sort()
    return names


def _invalid_record(review_id, reason):
    return {
        'review_id': review_id,
        'valid': False,
        'status': 'invalid',
        'reason': reason,
        'applied': False,
    }


def _read_pinned_fd_chunked(fd, before_st, *, cancel, deadline):
    """Read one already-pinned regular file fd in bounded chunks with abort checks."""
    data = bytearray()
    while True:
        _check_cancel_or_deadline(cancel, deadline)
        remaining = MAX_REVIEW_FILE_BYTES + 1 - len(data)
        if remaining <= 0:
            return None, 'verification_failed'
        to_read = min(READ_CHUNK_BYTES, remaining)
        try:
            chunk = os.read(fd, to_read)
        except OSError:
            return None, 'verification_failed'
        _check_cancel_or_deadline(cancel, deadline)
        if not chunk:
            break
        data.extend(chunk)
        if len(data) > MAX_REVIEW_FILE_BYTES:
            return None, 'verification_failed'

    try:
        after_st = os.fstat(fd)
    except OSError:
        return None, 'verification_failed'

    if len(data) > MAX_REVIEW_FILE_BYTES or len(data) != after_st.st_size:
        return None, 'verification_failed'
    if (
        before_st.st_dev,
        before_st.st_ino,
        before_st.st_size,
        before_st.st_mtime_ns,
        before_st.st_ctime_ns,
        before_st.st_nlink,
        before_st.st_mode,
        before_st.st_uid,
    ) != (
        after_st.st_dev,
        after_st.st_ino,
        after_st.st_size,
        after_st.st_mtime_ns,
        after_st.st_ctime_ns,
        after_st.st_nlink,
        after_st.st_mode,
        after_st.st_uid,
    ):
        return None, 'verification_failed'
    return bytes(data), None


def _sha256_chunked(data, *, cancel, deadline):
    """Compute SHA-256 over bytes in fixed chunks, checking cancel/deadline each chunk."""
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


def inspect_pinned_review_workspace(
    storage_fd,
    review_id,
    *,
    storage_dev=None,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
    deadline=None,
):
    """Inspect one review workspace solely via pinned directory FDs; metadata only.

    Never reopens by path, never uses unbounded `os.listdir`, never exposes
    source contents, and never invents creation timestamps absent from the
    verified `navi-private-review-v1` manifest.
    """
    if type(storage_fd) is not int or storage_fd < 0:
        raise ProjectReadError('Invalid private review storage descriptor')
    if type(review_id) is not str or _REVIEW_ID_RE.fullmatch(review_id) is None:
        raise ProjectReadError('Invalid private review identifier')

    if deadline is None:
        timeout = _validate_timeout(timeout_seconds)
        effective_deadline = time.monotonic() + timeout
    else:
        if (
            type(deadline) is bool
            or not isinstance(deadline, (int, float))
            or not math.isfinite(deadline)
        ):
            raise ProjectReadError('Invalid inventory deadline')
        effective_deadline = float(deadline)

    _check_cancel_or_deadline(cancel, effective_deadline)
    uid = os.getuid()

    try:
        storage_st = os.fstat(storage_fd)
    except OSError as error:
        raise ProjectReadError('Private review storage descriptor is unavailable') from error
    if (
        not stat.S_ISDIR(storage_st.st_mode)
        or storage_st.st_uid != uid
        or stat.S_IMODE(storage_st.st_mode) != 0o700
    ):
        raise ProjectReadError('Private review storage is not owner-private')
    if storage_dev is not None and storage_st.st_dev != storage_dev:
        raise ProjectReadError('Private review storage device mismatch')
    resolved_storage_dev = storage_st.st_dev

    _check_cancel_or_deadline(cancel, effective_deadline)
    try:
        top_st = os.lstat(review_id, dir_fd=storage_fd)
    except OSError:
        return _invalid_record(review_id, 'verification_failed')

    if stat.S_ISLNK(top_st.st_mode) or not stat.S_ISDIR(top_st.st_mode):
        return _invalid_record(review_id, 'invalid_entry_type')
    if (
        top_st.st_dev != resolved_storage_dev
        or top_st.st_uid != uid
        or stat.S_IMODE(top_st.st_mode) != 0o700
    ):
        return _invalid_record(review_id, 'invalid_permissions')

    _check_cancel_or_deadline(cancel, effective_deadline)
    child_fd = None
    try:
        try:
            child_fd = os.open(
                review_id,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                dir_fd=storage_fd,
            )
        except FileNotFoundError:
            return _invalid_record(review_id, 'verification_failed')
        except PermissionError:
            return _invalid_record(review_id, 'invalid_permissions')
        except OSError as error:
            if error.errno in (errno.ELOOP, errno.ENOTDIR):
                return _invalid_record(review_id, 'invalid_entry_type')
            return _invalid_record(review_id, 'verification_failed')

        _check_cancel_or_deadline(cancel, effective_deadline)
        try:
            dir_before = os.fstat(child_fd)
        except OSError:
            return _invalid_record(review_id, 'verification_failed')

        if not stat.S_ISDIR(dir_before.st_mode):
            return _invalid_record(review_id, 'invalid_entry_type')
        if (
            dir_before.st_dev != resolved_storage_dev
            or dir_before.st_uid != uid
            or stat.S_IMODE(dir_before.st_mode) != 0o700
        ):
            return _invalid_record(review_id, 'invalid_permissions')
        if dir_before.st_ino != top_st.st_ino:
            return _invalid_record(review_id, 'verification_failed')

        # Bounded inner scan directly on pinned child_fd: stops at entry #4.
        inner_names = []
        try:
            with os.scandir(child_fd) as entries:
                for entry in entries:
                    _check_cancel_or_deadline(cancel, effective_deadline)
                    if len(inner_names) >= len(FIXED_REVIEW_FILES):
                        return _invalid_record(review_id, 'incomplete_or_extra_files')
                    inner_names.append(entry.name)
        except OSError:
            return _invalid_record(review_id, 'verification_failed')

        if frozenset(inner_names) != FIXED_REVIEW_FILE_SET:
            return _invalid_record(review_id, 'incomplete_or_extra_files')

        opened_fds = []
        pinned_files = []
        raw_files = {}
        try:
            for filename in FIXED_REVIEW_FILES:
                _check_cancel_or_deadline(cancel, effective_deadline)
                try:
                    file_st = os.lstat(filename, dir_fd=child_fd)
                except OSError:
                    return _invalid_record(review_id, 'verification_failed')
                if stat.S_ISLNK(file_st.st_mode) or not stat.S_ISREG(file_st.st_mode):
                    return _invalid_record(review_id, 'invalid_entry_type')
                if (
                    file_st.st_dev != resolved_storage_dev
                    or file_st.st_uid != uid
                    or file_st.st_nlink != 1
                    or stat.S_IMODE(file_st.st_mode) != 0o600
                ):
                    return _invalid_record(review_id, 'invalid_permissions')
                if file_st.st_size > MAX_REVIEW_FILE_BYTES:
                    return _invalid_record(review_id, 'verification_failed')

                _check_cancel_or_deadline(cancel, effective_deadline)
                try:
                    file_fd = os.open(
                        filename,
                        os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC,
                        dir_fd=child_fd,
                    )
                except FileNotFoundError:
                    return _invalid_record(review_id, 'verification_failed')
                except PermissionError:
                    return _invalid_record(review_id, 'invalid_permissions')
                except OSError as error:
                    if error.errno in (errno.ELOOP, errno.EISDIR, errno.ENXIO, errno.ENOTDIR):
                        return _invalid_record(review_id, 'invalid_entry_type')
                    return _invalid_record(review_id, 'verification_failed')
                opened_fds.append(file_fd)

                try:
                    before_st = os.fstat(file_fd)
                except OSError:
                    return _invalid_record(review_id, 'verification_failed')
                if not stat.S_ISREG(before_st.st_mode):
                    return _invalid_record(review_id, 'invalid_entry_type')
                if (
                    before_st.st_dev != resolved_storage_dev
                    or before_st.st_uid != uid
                    or before_st.st_nlink != 1
                    or stat.S_IMODE(before_st.st_mode) != 0o600
                ):
                    return _invalid_record(review_id, 'invalid_permissions')
                if before_st.st_ino != file_st.st_ino or before_st.st_size > MAX_REVIEW_FILE_BYTES:
                    return _invalid_record(review_id, 'verification_failed')

                pinned_files.append((filename, file_fd, before_st))

            for filename, file_fd, before_st in pinned_files:
                payload, err_reason = _read_pinned_fd_chunked(
                    file_fd,
                    before_st,
                    cancel=cancel,
                    deadline=effective_deadline,
                )
                if err_reason is not None:
                    return _invalid_record(review_id, err_reason)
                raw_files[filename] = payload
        finally:
            for file_fd in opened_fds:
                try:
                    os.close(file_fd)
                except OSError:
                    pass

        try:
            dir_after = os.fstat(child_fd)
        except OSError:
            return _invalid_record(review_id, 'verification_failed')
        if (
            dir_before.st_dev,
            dir_before.st_ino,
            dir_before.st_uid,
            dir_before.st_mode,
            dir_before.st_nlink,
            dir_before.st_mtime_ns,
            dir_before.st_ctime_ns,
        ) != (
            dir_after.st_dev,
            dir_after.st_ino,
            dir_after.st_uid,
            dir_after.st_mode,
            dir_after.st_nlink,
            dir_after.st_mtime_ns,
            dir_after.st_ctime_ns,
        ):
            return _invalid_record(review_id, 'verification_failed')
    finally:
        if child_fd is not None:
            os.close(child_fd)

    _check_cancel_or_deadline(cancel, effective_deadline)
    before_raw = raw_files['before.txt']
    after_raw = raw_files['proposed-after.txt']
    manifest_raw = raw_files['review.json']

    try:
        manifest_text = manifest_raw.decode('utf-8')
        before_text = before_raw.decode('utf-8')
        after_text = after_raw.decode('utf-8')
        manifest = json.loads(
            manifest_text,
            object_pairs_hook=_strict_json_pairs,
            parse_constant=_reject_json_constant,
        )
    except (UnicodeError, ValueError, TypeError, RecursionError):
        return _invalid_record(review_id, 'verification_failed')

    if (
        type(manifest) is not dict
        or frozenset(manifest) != MANIFEST_KEYS
        or manifest['schema'] != REVIEW_MANIFEST_SCHEMA
        or manifest['applied'] is not False
        or manifest['requires_fresh_validation'] is not True
    ):
        return _invalid_record(review_id, 'verification_failed')

    original_path = manifest['original_path']
    manifest_before_sha = manifest['before_sha256']
    manifest_after_sha = manifest['after_sha256']
    manifest_digest = manifest['proposal_digest']
    notice = manifest['notice']
    if (
        type(original_path) is not str
        or not original_path
        or _UNSAFE_TEXT_RE.search(original_path) is not None
        or type(manifest_before_sha) is not str
        or _HEX64_RE.fullmatch(manifest_before_sha) is None
        or type(manifest_after_sha) is not str
        or _HEX64_RE.fullmatch(manifest_after_sha) is None
        or type(manifest_digest) is not str
        or _HEX64_RE.fullmatch(manifest_digest) is None
        or type(notice) is not str
    ):
        return _invalid_record(review_id, 'verification_failed')

    before_sha = _sha256_chunked(before_raw, cancel=cancel, deadline=effective_deadline)
    if before_sha != manifest_before_sha:
        return _invalid_record(review_id, 'verification_failed')

    after_sha = _sha256_chunked(after_raw, cancel=cancel, deadline=effective_deadline)
    if after_sha != manifest_after_sha:
        return _invalid_record(review_id, 'verification_failed')

    _check_cancel_or_deadline(cancel, effective_deadline)
    try:
        proposal = propose_edit(original_path, before_sha, before_text, after_text)
        _check_cancel_or_deadline(cancel, effective_deadline)
        digest = proposal_digest(proposal)
        _check_cancel_or_deadline(cancel, effective_deadline)
    except _InventoryAborted:
        raise
    except (ProjectReadError, ValueError, TypeError, KeyError, UnicodeError, RecursionError):
        _check_cancel_or_deadline(cancel, effective_deadline)
        return _invalid_record(review_id, 'verification_failed')

    if (
        proposal.path != original_path
        or proposal.before_sha256 != before_sha
        or proposal.after_sha256 != after_sha
        or type(digest) is not str
        or _HEX64_RE.fullmatch(digest) is None
        or digest != manifest_digest
    ):
        return _invalid_record(review_id, 'verification_failed')

    return {
        'review_id': review_id,
        'valid': True,
        'status': 'valid',
        'applied': False,
        'requires_fresh_validation': True,
        'original_path': proposal.path,
        'proposal_digest': digest,
        'before_sha256': before_sha,
        'after_sha256': after_sha,
        'before_bytes': len(before_raw),
        'after_bytes': len(after_raw),
        'files': list(FIXED_REVIEW_FILES),
    }


def serialize_inventory_result(result, *, max_bytes=None):
    """Serialize an inventory response to one capped ASCII JSON line."""
    cap = MAX_OUTPUT_BYTES if max_bytes is None else max_bytes
    if type(cap) is not int or cap <= 0:
        raise ProjectReadError('Invalid inventory output size bound')
    try:
        line = json.dumps(result, ensure_ascii=True, allow_nan=False) + '\n'
    except (TypeError, ValueError) as error:
        raise ProjectReadError('Invalid inventory output payload') from error
    if len(line.encode('ascii')) > cap:
        raise ProjectReadError('Private review inventory output exceeded size bound')
    return line


def list_review_inventory(data_root, *, cancel=None, timeout_seconds=DEFAULT_TIMEOUT_SECONDS):
    """Inspect `<data_root>/proposal-reviews` read-only and return bounded metadata only."""
    timeout = _validate_timeout(timeout_seconds)
    deadline = time.monotonic() + timeout
    _check_cancel_or_deadline(cancel, deadline)

    storage_fd = _open_existing_review_storage(data_root, cancel=cancel, deadline=deadline)
    if storage_fd is None:
        _check_cancel_or_deadline(cancel, deadline)
        result = {
            'schema': INVENTORY_SCHEMA,
            'applied': False,
            'source_contents_exposed': False,
            'storage_present': False,
            'inspected_count': 0,
            'valid_count': 0,
            'invalid_count': 0,
            'max_inspected_children': MAX_INSPECTED_CHILDREN,
            'max_valid_records': MAX_VALID_RECORDS,
            'items': [],
            'notice': _INVENTORY_NOTICE,
        }
        serialize_inventory_result(result)
        return result

    try:
        storage_dev = os.fstat(storage_fd).st_dev
        names = _bounded_scandir_names(
            storage_fd, MAX_INSPECTED_CHILDREN, cancel=cancel, deadline=deadline
        )

        # Refuse unexpected non-review entries fail-closed without following or echoing names.
        for name in names:
            _check_cancel_or_deadline(cancel, deadline)
            if _REVIEW_ID_RE.fullmatch(name) is None:
                raise ProjectReadError('Unexpected entry in private review storage')

        items = []
        valid_count = 0
        invalid_count = 0
        running_item_bytes = 0

        for review_id in names:
            _check_cancel_or_deadline(cancel, deadline)
            record = inspect_pinned_review_workspace(
                storage_fd,
                review_id,
                storage_dev=storage_dev,
                cancel=cancel,
                deadline=deadline,
            )
            if record['valid']:
                valid_count += 1
                if valid_count > MAX_VALID_RECORDS:
                    raise ProjectReadError(
                        'Private review inventory exceeded 32 valid record cap'
                    )
            else:
                invalid_count += 1

            items.append(record)
            running_item_bytes += (
                len(json.dumps(record, ensure_ascii=True, allow_nan=False)) + 2
            )
            if running_item_bytes > MAX_OUTPUT_BYTES:
                raise ProjectReadError('Private review inventory output exceeded size bound')

        _check_cancel_or_deadline(cancel, deadline)
        result = {
            'schema': INVENTORY_SCHEMA,
            'applied': False,
            'source_contents_exposed': False,
            'storage_present': True,
            'inspected_count': len(items),
            'valid_count': valid_count,
            'invalid_count': invalid_count,
            'max_inspected_children': MAX_INSPECTED_CHILDREN,
            'max_valid_records': MAX_VALID_RECORDS,
            'items': items,
            'notice': _INVENTORY_NOTICE,
        }
        serialize_inventory_result(result)
        return result
    finally:
        os.close(storage_fd)


def decode_inventory_frame(frame):
    """Decode one fixed inventory trigger frame. Rejects roots, paths, or commands."""
    if type(frame) is not bytes or len(frame) > MAX_REQUEST_FRAME_BYTES or not frame.endswith(b'\n'):
        raise ProjectReadError('Invalid review inventory request frame')

    try:
        value = json.loads(
            frame,
            object_pairs_hook=_strict_json_pairs,
            parse_constant=_reject_json_constant,
        )
        if (
            type(value) is not dict
            or set(value) != {'schema'}
            or value['schema'] != REQUEST_SCHEMA
        ):
            raise ValueError()
    except (ValueError, TypeError, UnicodeError, RecursionError) as error:
        raise ProjectReadError('Invalid review inventory request') from error
    return value


def inventory_request(data_root, frame, *, cancel=None, timeout_seconds=DEFAULT_TIMEOUT_SECONDS):
    decode_inventory_frame(frame)
    return list_review_inventory(data_root, cancel=cancel, timeout_seconds=timeout_seconds)


def main():
    cancel = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: cancel.set())
    signal.signal(signal.SIGINT, lambda *_: cancel.set())
    try:
        if len(sys.argv) != 3 or sys.argv[1] != '--owner-data':
            raise ProjectReadError('Fixed --owner-data argument required')
        frame = sys.stdin.buffer.readline(MAX_REQUEST_FRAME_BYTES + 1)
        if cancel.is_set():
            return 1
        result = inventory_request(sys.argv[2], frame, cancel=cancel)
        encoded = serialize_inventory_result(result)
        if cancel.is_set():
            return 1
        sys.stdout.write(encoded)
        sys.stdout.flush()
        return 0
    except (ProjectReadError, OSError):
        print(
            json.dumps(
                {
                    'error': 'Private review inventory unavailable, exceeded bounds, or canceled',
                    'applied': False,
                    'source_contents_exposed': False,
                },
                ensure_ascii=True,
            ),
            flush=True,
        )
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
