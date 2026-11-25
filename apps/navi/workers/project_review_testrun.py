# apps/navi/workers/project_review_testrun.py
import json
import os
import shutil
import stat
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

import project_review_diff as _diff
import project_test_sandbox

_PreviewFailure = _diff._PreviewFailure
_inspect_and_read_review = _diff._inspect_and_read_review
_check_workspace_stale = _diff._check_workspace_stale
_validate_root_path = _diff._validate_root_path
_safe_review_id = _diff._safe_review_id
_is_sha256_hex = _diff._is_sha256_hex
_resolve_deadline = _diff._resolve_deadline
_map_external_exception = _diff._map_external_exception
_check_cancel_or_deadline = _diff._check_cancel_or_deadline

REASON_INVALID_REQUEST = _diff.REASON_INVALID_REQUEST
REASON_NOT_FOUND = _diff.REASON_NOT_FOUND
REASON_DIGEST_MISMATCH = _diff.REASON_DIGEST_MISMATCH
REASON_VERIFICATION_FAILED = _diff.REASON_VERIFICATION_FAILED
REASON_INVALID_ENTRY_TYPE = _diff.REASON_INVALID_ENTRY_TYPE
REASON_INVALID_PERMISSIONS = _diff.REASON_INVALID_PERMISSIONS
REASON_TOO_LARGE = _diff.REASON_TOO_LARGE
REASON_INVALID_STORAGE = _diff.REASON_INVALID_STORAGE
REASON_CANCELED = _diff.REASON_CANCELED
REASON_TIMED_OUT = _diff.REASON_TIMED_OUT
REASON_STALE_TARGET = "stale_target"
REASON_TREE_TOO_LARGE = "tree_too_large"
REASON_COPY_FAILED = "copy_failed"
REASON_SANDBOX_UNAVAILABLE = "sandbox_unavailable"
REASON_BUSY = "busy"
REASON_OUTPUT_TOO_LARGE = "output_too_large"
REASON_FAILED_TO_START = "failed_to_start"

SANDBOX_FAILURE_REASONS = frozenset({
    REASON_INVALID_REQUEST,
    REASON_SANDBOX_UNAVAILABLE,
    REASON_BUSY,
    REASON_TIMED_OUT,
    REASON_OUTPUT_TOO_LARGE,
    REASON_FAILED_TO_START,
})
VALID_REASONS = frozenset({
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
    REASON_STALE_TARGET,
    REASON_TREE_TOO_LARGE,
    REASON_COPY_FAILED,
    REASON_SANDBOX_UNAVAILABLE,
    REASON_BUSY,
    REASON_OUTPUT_TOO_LARGE,
    REASON_FAILED_TO_START,
})

REQUEST_SCHEMA = "navi-review-testrun-request-v1"
REPLY_SCHEMA = "navi-review-testrun-v1"
SANDBOX_REQUEST_SCHEMA = "navi-test-sandbox-request-v1"
EXPECTED_REQUEST_KEYS = frozenset({
    "schema",
    "data_root",
    "workspace_root",
    "review_id",
    "proposal_digest",
})

MAX_REQUEST_BYTES = 16384
MAX_REPLY_BYTES = 65536
MAX_TAIL_CHARS = 4000
MAX_FILES = 3000
MAX_TOTAL_BYTES = 24 * 1024 * 1024
MAX_DEPTH = 12
SANDBOX_TIMEOUT_SECONDS = 30
DEFAULT_TIMEOUT_SECONDS = 45.0
COPY_CHUNK_BYTES = 65536
TEMP_PREFIX = "navi-testrun-"

SKIP_NAMES = frozenset({
    ".git",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".env",
})
SKIP_PREFIXES = (".env", ".ssh", ".aws", ".npmrc", ".pypirc", ".netrc")
SKIP_SUBSTRINGS = ("secret", "credential", "id_rsa")

_O_NOFOLLOW = getattr(os, "O_NOFOLLOW", 0)
_O_DIRECTORY = getattr(os, "O_DIRECTORY", 0)
_O_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
_O_NONBLOCK = getattr(os, "O_NONBLOCK", 0)


class _RequestError(_PreviewFailure):
    def __init__(self, reason, review_id=None):
        super().__init__(reason)
        self.review_id = _safe_review_id(review_id)


def _failure(review_id, reason):
    safe_reason = reason if reason in VALID_REASONS else REASON_VERIFICATION_FAILED
    return {
        "schema": REPLY_SCHEMA,
        "review_id": _safe_review_id(review_id),
        "ok": False,
        "reason": safe_reason,
    }


def _encode_reply(reply):
    return (
        json.dumps(reply, separators=(",", ":"), ensure_ascii=False, allow_nan=False)
        + "\n"
    ).encode("utf-8")


def _should_skip_name(name):
    lower = name.lower()
    return (
        lower in SKIP_NAMES
        or lower.startswith(SKIP_PREFIXES)
        or any(token in lower for token in SKIP_SUBSTRINGS)
    )


def _write_all(fd, data):
    view = memoryview(data)
    written = 0
    while written < len(view):
        count = os.write(fd, view[written:])
        if count <= 0:
            raise OSError("short write")
        written += count


def _split_original_path(original_path):
    if (
        not isinstance(original_path, str)
        or not original_path
        or original_path.startswith("/")
        or "\\" in original_path
        or "\x00" in original_path
    ):
        raise _PreviewFailure(REASON_INVALID_STORAGE)
    parts = original_path.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise _PreviewFailure(REASON_INVALID_STORAGE)
    return parts


def copy_workspace_tree(
    workspace_root,
    dst_root,
    *,
    cancel=None,
    deadline=None,
    max_files=MAX_FILES,
    max_bytes=MAX_TOTAL_BYTES,
    max_depth=MAX_DEPTH,
):
    state = [0, 0]
    seen = set()

    def _walk(src_fd, dst_fd, depth):
        _check_cancel_or_deadline(cancel, deadline)
        with os.scandir(src_fd) as entries:
            for entry in entries:
                _check_cancel_or_deadline(cancel, deadline)
                name = entry.name
                if not name or name in (".", "..") or _should_skip_name(name):
                    continue
                if entry.is_symlink():
                    continue
                if entry.is_dir(follow_symlinks=False):
                    if depth >= max_depth:
                        raise _PreviewFailure(REASON_TREE_TOO_LARGE)
                    try:
                        c_sfd = os.open(
                            name,
                            os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC,
                            dir_fd=src_fd,
                        )
                    except OSError as err:
                        if err.errno in (40, 20, 2):
                            continue
                        raise
                    key = None
                    c_dfd = None
                    try:
                        cst = os.fstat(c_sfd)
                        if not stat.S_ISDIR(cst.st_mode):
                            continue
                        key = (cst.st_dev, cst.st_ino)
                        if key in seen:
                            key = None
                            continue
                        seen.add(key)
                        os.mkdir(name, 0o700, dir_fd=dst_fd)
                        c_dfd = os.open(
                            name,
                            os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC,
                            dir_fd=dst_fd,
                        )
                        os.fchmod(c_dfd, 0o700)
                        _walk(c_sfd, c_dfd, depth + 1)
                    finally:
                        if c_dfd is not None:
                            os.close(c_dfd)
                        os.close(c_sfd)
                        if key is not None:
                            seen.discard(key)
                elif entry.is_file(follow_symlinks=False):
                    if state[0] + 1 > max_files:
                        raise _PreviewFailure(REASON_TREE_TOO_LARGE)
                    try:
                        c_sfd = os.open(
                            name,
                            os.O_RDONLY | _O_NOFOLLOW | _O_NONBLOCK | _O_CLOEXEC,
                            dir_fd=src_fd,
                        )
                    except OSError as err:
                        if err.errno in (40, 2):
                            continue
                        raise
                    c_dfd = None
                    try:
                        cst = os.fstat(c_sfd)
                        if not stat.S_ISREG(cst.st_mode):
                            continue
                        state[0] += 1
                        if state[1] + cst.st_size > max_bytes:
                            raise _PreviewFailure(REASON_TREE_TOO_LARGE)
                        c_dfd = os.open(
                            name,
                            os.O_WRONLY | os.O_CREAT | os.O_EXCL | _O_NOFOLLOW | _O_CLOEXEC,
                            0o600,
                            dir_fd=dst_fd,
                        )
                        os.fchmod(c_dfd, 0o600)
                        while True:
                            _check_cancel_or_deadline(cancel, deadline)
                            chunk = os.read(c_sfd, COPY_CHUNK_BYTES)
                            if not chunk:
                                break
                            state[1] += len(chunk)
                            if state[1] > max_bytes:
                                raise _PreviewFailure(REASON_TREE_TOO_LARGE)
                            _write_all(c_dfd, chunk)
                    finally:
                        if c_dfd is not None:
                            os.close(c_dfd)
                        os.close(c_sfd)

    src_root_fd = None
    dst_root_fd = None
    try:
        src_root_fd = os.open(
            workspace_root, os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC
        )
        root_st = os.fstat(src_root_fd)
        if not stat.S_ISDIR(root_st.st_mode):
            raise _PreviewFailure(REASON_COPY_FAILED)
        seen.add((root_st.st_dev, root_st.st_ino))
        dst_root_fd = os.open(
            dst_root, os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC
        )
        _walk(src_root_fd, dst_root_fd, 0)
        return state[0], state[1]
    except _PreviewFailure:
        raise
    except Exception as error:
        raise _PreviewFailure(
            _map_external_exception(
                error, cancel=cancel, deadline=deadline, default_reason=REASON_COPY_FAILED
            )
        ) from error
    finally:
        if dst_root_fd is not None:
            os.close(dst_root_fd)
        if src_root_fd is not None:
            os.close(src_root_fd)


def _apply_proposed_edit(dst_root, original_path, after_data):
    parts = _split_original_path(original_path)
    if not isinstance(after_data, (bytes, bytearray)):
        raise _PreviewFailure(REASON_INVALID_STORAGE)
    cur_fd = None
    target_fd = None
    try:
        cur_fd = os.open(
            dst_root, os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC
        )
        for comp in parts[:-1]:
            try:
                os.mkdir(comp, 0o700, dir_fd=cur_fd)
            except FileExistsError:
                pass
            nxt_fd = os.open(
                comp,
                os.O_RDONLY | _O_DIRECTORY | _O_NOFOLLOW | _O_CLOEXEC,
                dir_fd=cur_fd,
            )
            os.fchmod(nxt_fd, 0o700)
            os.close(cur_fd)
            cur_fd = nxt_fd
        target_fd = os.open(
            parts[-1],
            os.O_WRONLY | os.O_CREAT | os.O_TRUNC | _O_NOFOLLOW | _O_CLOEXEC,
            0o600,
            dir_fd=cur_fd,
        )
        if not stat.S_ISREG(os.fstat(target_fd).st_mode):
            raise _PreviewFailure(REASON_COPY_FAILED)
        os.fchmod(target_fd, 0o600)
        _write_all(target_fd, after_data)
    except _PreviewFailure:
        raise
    except OSError as error:
        raise _PreviewFailure(REASON_COPY_FAILED) from error
    finally:
        if target_fd is not None:
            os.close(target_fd)
        if cur_fd is not None:
            os.close(cur_fd)


def _cleanup_temp_dir(dst_root, expected_id, uid):
    try:
        if (
            not isinstance(dst_root, str)
            or not dst_root
            or not os.path.isabs(dst_root)
            or os.path.islink(dst_root)
            or os.path.realpath(dst_root) != dst_root
            or not os.path.basename(dst_root).startswith(TEMP_PREFIX)
        ):
            return
        st = os.lstat(dst_root)
        if (
            not stat.S_ISDIR(st.st_mode)
            or st.st_uid != uid
            or (expected_id is not None and (st.st_dev, st.st_ino) != expected_id)
        ):
            return
        shutil.rmtree(dst_root)
    except OSError:
        pass


def _choose_test_argv_id(dst_root):
    try:
        st = os.lstat(os.path.join(dst_root, "tests"))
        if stat.S_ISDIR(st.st_mode):
            return "unittest_discover_tests"
    except OSError:
        pass
    return "unittest_discover_root"


def _scrub_tail(text, secret_paths):
    for path in secret_paths:
        if path and len(path) > 1 and path in text:
            text = text.replace(path, "<redacted>")
    if len(text) > MAX_TAIL_CHARS:
        return text[-MAX_TAIL_CHARS:]
    return text


def _map_sandbox_reply(review_id, sandbox_reply, secret_paths):
    if not isinstance(sandbox_reply, dict):
        return _failure(review_id, REASON_SANDBOX_UNAVAILABLE)
    if sandbox_reply.get("ok") is True:
        passed = sandbox_reply.get("passed")
        exit_code = sandbox_reply.get("exit_code")
        stdout_tail = sandbox_reply.get("stdout_tail")
        stderr_tail = sandbox_reply.get("stderr_tail")
        if (
            not isinstance(passed, bool)
            or (exit_code is not None and (isinstance(exit_code, bool) or not isinstance(exit_code, int)))
            or not isinstance(stdout_tail, str)
            or not isinstance(stderr_tail, str)
        ):
            return _failure(review_id, REASON_SANDBOX_UNAVAILABLE)
        return {
            "schema": REPLY_SCHEMA,
            "review_id": _safe_review_id(review_id),
            "ok": True,
            "passed": passed,
            "exit_code": exit_code,
            "stdout_tail": _scrub_tail(stdout_tail, secret_paths),
            "stderr_tail": _scrub_tail(stderr_tail, secret_paths),
        }
    reason = sandbox_reply.get("reason")
    if reason not in SANDBOX_FAILURE_REASONS:
        reason = REASON_SANDBOX_UNAVAILABLE
    return _failure(review_id, reason)


def run_review_testrun(
    data_root,
    workspace_root,
    review_id,
    proposal_digest,
    *,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
    deadline=None,
    cancel=None,
):
    safe_id = _safe_review_id(review_id)
    norm_data = _validate_root_path(data_root)
    norm_ws = _validate_root_path(workspace_root)
    if (
        norm_data is None
        or norm_ws is None
        or safe_id is None
        or not _is_sha256_hex(proposal_digest)
    ):
        return _failure(safe_id, REASON_INVALID_REQUEST)

    uid = os.geteuid()
    dst = None
    dst_id = None
    try:
        resolved_deadline = _resolve_deadline(timeout_seconds, deadline)
        _check_cancel_or_deadline(cancel, resolved_deadline)
        original_path, before_sha256, before_data, after_data = _inspect_and_read_review(
            norm_data,
            safe_id,
            proposal_digest,
            uid=uid,
            cancel=cancel,
            deadline=resolved_deadline,
        )
        if _check_workspace_stale(
            norm_ws,
            original_path,
            expected_sha256=before_sha256,
            expected_bytes=len(before_data),
            uid=uid,
            cancel=cancel,
            deadline=resolved_deadline,
        ):
            return _failure(safe_id, REASON_STALE_TARGET)
        _split_original_path(original_path)

        raw_dst = tempfile.mkdtemp(prefix=TEMP_PREFIX)
        dst = os.path.realpath(raw_dst)
        os.chmod(dst, 0o700)
        dst_st = os.lstat(dst)
        if not stat.S_ISDIR(dst_st.st_mode) or dst_st.st_uid != uid:
            raise _PreviewFailure(REASON_COPY_FAILED)
        dst_id = (dst_st.st_dev, dst_st.st_ino)

        copy_workspace_tree(norm_ws, dst, cancel=cancel, deadline=resolved_deadline)
        _apply_proposed_edit(dst, original_path, after_data)
        _check_cancel_or_deadline(cancel, resolved_deadline)

        sandbox_req = json.dumps(
            {
                "schema": SANDBOX_REQUEST_SCHEMA,
                "tree_root": dst,
                "test_argv_id": _choose_test_argv_id(dst),
                "timeout_seconds": SANDBOX_TIMEOUT_SECONDS,
            },
            separators=(",", ":"),
            allow_nan=False,
        ).encode("utf-8")
        try:
            sandbox_reply = project_test_sandbox.handle_request(sandbox_req)
        except Exception as error:
            raise _PreviewFailure(
                _map_external_exception(
                    error,
                    cancel=cancel,
                    deadline=resolved_deadline,
                    default_reason=REASON_SANDBOX_UNAVAILABLE,
                )
            ) from error

        secret_paths = sorted(
            {
                p
                for p in (
                    dst,
                    raw_dst,
                    norm_ws,
                    os.path.realpath(norm_ws),
                    norm_data,
                    os.path.realpath(norm_data),
                )
                if p and len(p) > 1
            },
            key=len,
            reverse=True,
        )
        return _map_sandbox_reply(safe_id, sandbox_reply, secret_paths)
    except _PreviewFailure as error:
        return _failure(safe_id, error.reason)
    except Exception as error:
        return _failure(
            safe_id,
            _map_external_exception(
                error,
                cancel=cancel,
                deadline=deadline,
                default_reason=REASON_VERIFICATION_FAILED,
            ),
        )
    finally:
        if dst is not None:
            _cleanup_temp_dir(dst, dst_id, uid)


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
            reply = run_review_testrun(
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
