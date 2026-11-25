"""Preparatory bounded candidate-snapshot and output-cap helpers for Python unittest.

AGGREGATE SANDBOX EXECUTION STATUS: NOT_IMPLEMENTED.

Public execution entrypoints (`run_candidate_unittest`, `candidate_test_request`,
and `main`) unconditionally fail closed BEFORE reading any project files or
creating private candidate directories. Enabling actual project execution is
blocked pending:
1. A separately reviewed exclusive cgroup v2 launcher (preventing aggregate
   fork/memory storms and eliminating shared-leaf foreign-process kill races),
2. An explicit owner provisioning decision for that cgroup launcher,
3. Host capped-tmpfs support (`bwrap --size` on bubblewrap >= 0.7.0), and
4. A fresh daemon-bound owner whole-project snapshot approval route tied to
   active project, generation/epoch, and snapshot extent.

No cgroup filesystem write helpers, enable env vars, bypass flags, or project
execution routes exist in this module. `_build_candidate_snapshot` is an
internal preparatory API for synthetic tests only; its `whole_project_snapshot_approved`
boolean is a defensive precondition, NOT approval authority.
"""
import hashlib
import json
import math
import os
from pathlib import Path
import re
import secrets
import selectors
import signal
import stat
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from project_edit_reviews import proposal_digest
from project_edits import EditProposal, ProjectReadError, propose_edit
from project_validate import decode_preview_frame

AGGREGATE_SANDBOX_STATUS = 'NOT_IMPLEMENTED'
CANDIDATE_EXECUTION_UNAVAILABLE = (
    'Candidate execution is not implemented and remains unavailable pending a '
    'separately reviewed exclusive cgroup launcher, owner provisioning decision, '
    'capped tmpfs support, and a fresh daemon-bound owner whole-project snapshot '
    'approval route.'
)

RUNNER_NAME = 'python-unittest'
CANDIDATE_RUNS_DIR = 'candidate-runs'
ALLOWED_EXTENSIONS = ('.py', '.json', '.toml', '.txt')

MAX_SNAPSHOT_FILES = 256
MAX_SNAPSHOT_DIRS = 256
MAX_FILE_BYTES = 65536             # 64 KiB per eligible text file
MAX_TOTAL_BYTES = 4 * 1024 * 1024  # 4 MiB total candidate snapshot
MAX_VISITED_ENTRIES = 4096
MAX_DIR_DEPTH = 16
# Strictly exceeds the mathematical maximum (MAX_SNAPSHOT_FILES + MAX_SNAPSHOT_DIRS = 512).
MAX_CLEANUP_ENTRIES = MAX_SNAPSHOT_FILES + MAX_SNAPSHOT_DIRS + 64

MAX_OUTPUT_BYTES = 32768           # 32 KiB combined stdout + stderr
MAX_FRAME_BYTES = 230000

DEFAULT_TIMEOUT_SECONDS = 10.0
MIN_TIMEOUT_SECONDS = 0.5
MAX_TIMEOUT_SECONDS = 15.0

_RUN_NAME_RE = re.compile(r'^run-[0-9a-f]{32}$')
_DECEPTIVE_RE = re.compile(r'[\x00-\x08\x0b\x0c\x0e-\x1f\x7f\u202a-\u202e\u2066-\u2069]')


def _check_cancel(cancel):
    if cancel is not None and cancel.is_set():
        raise ProjectReadError('Candidate preparation or bounded utility canceled')


def _sanitize_output(raw):
    text = bytes(raw).decode('utf-8', errors='replace')
    return _DECEPTIVE_RE.sub('', text)


def _open_owner_storage(root):
    if not isinstance(root, (str, os.PathLike)):
        raise ProjectReadError('Private owner storage must be an absolute path')
    path = Path(root)
    if not path.is_absolute() or '..' in path.parts or len(path.parts) <= 1:
        raise ProjectReadError('Private owner storage must be an absolute non-root path')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for component in path.parts[1:]:
            next_fd = os.open(
                component,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=fd,
            )
            os.close(fd)
            fd = next_fd
            info = os.fstat(fd)
            if info.st_mode & 0o022 or info.st_uid not in (0, os.getuid()):
                raise ProjectReadError('Candidate storage crosses an unsafe ancestor')
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
            raise ProjectReadError('Candidate storage must be owner-only')
        return fd
    except BaseException:
        os.close(fd)
        raise


def _open_project_root(root):
    if not isinstance(root, (str, os.PathLike)):
        raise ProjectReadError('Project root must be an absolute path')
    path = Path(root)
    if not path.is_absolute() or '..' in path.parts or len(path.parts) <= 1:
        raise ProjectReadError('Project root must be an absolute non-root path')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for component in path.parts[1:]:
            next_fd = os.open(
                component,
                os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=fd,
            )
            os.close(fd)
            fd = next_fd
            info = os.fstat(fd)
            if info.st_mode & 0o002 or info.st_uid not in (0, os.getuid()):
                raise ProjectReadError('Project root crosses an unsafe ancestor')
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or info.st_mode & 0o002:
            raise ProjectReadError('Project root must be owner-owned and not world-writable')
        return fd
    except BaseException:
        os.close(fd)
        raise


def owner_candidate_storage(data_root):
    """Create and verify the fixed candidate-runs directory under private owner data."""
    parent = _open_owner_storage(data_root)
    try:
        try:
            os.mkdir(CANDIDATE_RUNS_DIR, 0o700, dir_fd=parent)
        except FileExistsError:
            pass
        fd = os.open(
            CANDIDATE_RUNS_DIR,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=parent,
        )
        try:
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                raise ProjectReadError('Candidate storage is not owner-private')
        finally:
            os.close(fd)
        return Path(data_root) / CANDIDATE_RUNS_DIR
    finally:
        os.close(parent)


def _validate_proposal(proposal):
    if type(proposal) is not EditProposal:
        raise ProjectReadError('An exact prepared preview is required')
    rebuilt = propose_edit(
        proposal.path, proposal.before_sha256, proposal.before, proposal.after
    )
    if rebuilt != proposal:
        raise ProjectReadError('Preview integrity verification failed')
    raw_path = proposal.path
    if (
        not raw_path
        or raw_path.startswith('/')
        or raw_path.endswith('/')
        or '\\' in raw_path
        or '\x00' in raw_path
    ):
        raise ProjectReadError('Invalid preview target path')
    parts = raw_path.split('/')
    if len(parts) > MAX_DIR_DEPTH + 1:
        raise ProjectReadError('Preview target path exceeds nesting limit')
    for part in parts:
        if not part or part in ('.', '..') or part.startswith('.'):
            raise ProjectReadError('Hidden or non-canonical preview target path rejected')
    if Path(parts[-1]).suffix not in ALLOWED_EXTENSIONS:
        raise ProjectReadError('Preview target extension is not eligible for candidate snapshots')
    after_bytes = proposal.after.encode('utf-8')
    if b'\x00' in after_bytes:
        raise ProjectReadError('Preview replacement contains NUL bytes')
    if len(after_bytes) > MAX_FILE_BYTES:
        raise ProjectReadError('Preview replacement exceeds candidate file size cap')
    if hashlib.sha256(after_bytes).hexdigest() != proposal.after_sha256:
        raise ProjectReadError('Preview after_sha256 mismatch')
    return after_bytes


def _bounded_scandir_names(dir_fd, max_entries, *, cancel=None):
    """Stream at most max_entries names via os.scandir(fd), then sort that bounded list."""
    names = []
    with os.scandir(dir_fd) as entries:
        for entry in entries:
            _check_cancel(cancel)
            if len(names) >= max_entries:
                raise ProjectReadError('Project exceeds snapshot entry inspection cap')
            names.append(entry.name)
    names.sort()
    return names


def _ensure_candidate_subdir(cand_root_fd, rel_parts, state):
    cur_fd = os.dup(cand_root_fd)
    try:
        for part in rel_parts:
            try:
                next_fd = os.open(
                    part,
                    os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                    dir_fd=cur_fd,
                )
            except FileNotFoundError:
                if state['dirs_created'] >= MAX_SNAPSHOT_DIRS:
                    raise ProjectReadError('Project exceeds candidate directory creation cap')
                os.mkdir(part, 0o700, dir_fd=cur_fd)
                state['dirs_created'] += 1
                next_fd = os.open(
                    part,
                    os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                    dir_fd=cur_fd,
                )
            os.close(cur_fd)
            cur_fd = next_fd
            info = os.fstat(cur_fd)
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                raise ProjectReadError('Candidate subdirectory permissions invalid')
        return cur_fd
    except BaseException:
        os.close(cur_fd)
        raise


def _write_candidate_file(cand_root_fd, rel_parts, filename, data, state, cancel=None):
    dest_dir_fd = _ensure_candidate_subdir(cand_root_fd, rel_parts, state)
    try:
        fd = os.open(
            filename,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
            0o600,
            dir_fd=dest_dir_fd,
        )
        try:
            offset = 0
            while offset < len(data):
                _check_cancel(cancel)
                written = os.write(fd, data[offset:])
                if written <= 0:
                    raise ProjectReadError('Candidate file write failed')
                offset += written
        finally:
            os.close(fd)
    finally:
        os.close(dest_dir_fd)


def _read_regular_text_file(dir_fd, name, expected_stat, root_dev, cancel=None):
    fd = os.open(
        name,
        os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC,
        dir_fd=dir_fd,
    )
    try:
        before = os.fstat(fd)
        if (
            not stat.S_ISREG(before.st_mode)
            or before.st_nlink != 1
            or before.st_dev != root_dev
            or before.st_ino != expected_stat.st_ino
            or before.st_size > MAX_FILE_BYTES
        ):
            raise ProjectReadError('Project file failed regular-file or size checks')
        data = bytearray()
        while len(data) <= MAX_FILE_BYTES:
            _check_cancel(cancel)
            chunk = os.read(fd, (MAX_FILE_BYTES + 1) - len(data))
            if not chunk:
                break
            data.extend(chunk)
        after = os.fstat(fd)
        if len(data) > MAX_FILE_BYTES or (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        ) != (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        ):
            raise ProjectReadError('Project file changed or exceeded size cap during snapshot')
        raw = bytes(data)
        if b'\x00' in raw:
            raise ProjectReadError('Eligible project file contains NUL bytes')
        try:
            raw.decode('utf-8')
        except UnicodeDecodeError as error:
            raise ProjectReadError('Eligible project file is not valid UTF-8 text') from error
        return raw
    finally:
        os.close(fd)


def _cleanup_candidate_tree(parent_fd, run_name, *, best_effort=False):
    """Bounded dir_fd-only regular-file cleanup using scandir(fd). Never follows symlinks."""
    if not isinstance(run_name, str) or _RUN_NAME_RE.fullmatch(run_name) is None:
        if best_effort:
            return
        raise ProjectReadError('Invalid candidate workspace identifier')

    budget = [MAX_CLEANUP_ENTRIES]
    errors = []
    parent_dev = os.fstat(parent_fd).st_dev

    def _walk_remove(dir_fd, depth):
        if depth > MAX_DIR_DEPTH + 2:
            errors.append('Cleanup nesting exceeded bound')
            return
        names = []
        try:
            with os.scandir(dir_fd) as entries:
                for item in entries:
                    budget[0] -= 1
                    if budget[0] < 0:
                        errors.append('Cleanup entry count exceeded bound')
                        return
                    names.append(item.name)
        except OSError:
            errors.append('Cleanup directory scan failed')
            return
        for entry in names:
            try:
                st = os.lstat(entry, dir_fd=dir_fd)
            except FileNotFoundError:
                continue
            except OSError:
                errors.append('Cleanup lstat failed')
                continue
            if stat.S_ISREG(st.st_mode):
                try:
                    os.unlink(entry, dir_fd=dir_fd)
                except FileNotFoundError:
                    pass
                except OSError:
                    errors.append('Cleanup unlink failed')
            elif stat.S_ISDIR(st.st_mode):
                if st.st_dev != parent_dev or st.st_uid != os.getuid():
                    errors.append('Cleanup refused unsafe directory')
                    continue
                sub_fd = None
                try:
                    sub_fd = os.open(
                        entry,
                        os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                        dir_fd=dir_fd,
                    )
                    sub_st = os.fstat(sub_fd)
                    if (
                        not stat.S_ISDIR(sub_st.st_mode)
                        or sub_st.st_dev != parent_dev
                        or sub_st.st_ino != st.st_ino
                        or sub_st.st_uid != os.getuid()
                    ):
                        errors.append('Cleanup directory identity changed')
                        continue
                    _walk_remove(sub_fd, depth + 1)
                except OSError:
                    errors.append('Cleanup directory open failed')
                    continue
                finally:
                    if sub_fd is not None:
                        os.close(sub_fd)
                try:
                    os.rmdir(entry, dir_fd=dir_fd)
                except FileNotFoundError:
                    pass
                except OSError:
                    errors.append('Cleanup rmdir failed')
            else:
                errors.append('Cleanup refused symlink or non-regular file')

    run_fd = None
    try:
        run_fd = os.open(
            run_name,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=parent_fd,
        )
        run_st = os.fstat(run_fd)
        if (
            not stat.S_ISDIR(run_st.st_mode)
            or run_st.st_dev != parent_dev
            or run_st.st_uid != os.getuid()
        ):
            errors.append('Cleanup refused unsafe candidate root')
        else:
            _walk_remove(run_fd, 0)
    except FileNotFoundError:
        return
    except OSError:
        errors.append('Cleanup candidate root open failed')
    finally:
        if run_fd is not None:
            os.close(run_fd)

    if not errors:
        try:
            os.rmdir(run_name, dir_fd=parent_fd)
        except FileNotFoundError:
            pass
        except OSError:
            errors.append('Cleanup candidate root rmdir failed')
    elif best_effort:
        try:
            os.rmdir(run_name, dir_fd=parent_fd)
        except OSError:
            pass

    if errors and not best_effort:
        raise ProjectReadError('Candidate workspace cleanup failed')


def _cleanup_candidate_workspace(storage, run_name, *, best_effort=False):
    storage_fd = _open_owner_storage(storage)
    try:
        _cleanup_candidate_tree(storage_fd, run_name, best_effort=best_effort)
    finally:
        os.close(storage_fd)


def _build_candidate_snapshot(
    project_root,
    proposal,
    storage,
    *,
    run_name=None,
    whole_project_snapshot_approved=False,
    cancel=None,
):
    """Internal preparatory API for synthetic tests only.

    `whole_project_snapshot_approved` is a defensive precondition, NOT approval
    authority; a future daemon route must bind fresh owner approval to current
    project, generation/epoch, and snapshot extent. Never writes to project_root.
    Cleans up `run_name` on failure ONLY if this invocation created it.
    """
    if whole_project_snapshot_approved is not True:
        raise ProjectReadError('Explicit owner whole-project snapshot approval is required')
    _check_cancel(cancel)
    after_bytes = _validate_proposal(proposal)
    digest = proposal_digest(proposal)

    if run_name is None:
        run_name = 'run-' + secrets.token_hex(16)
    elif not isinstance(run_name, str) or _RUN_NAME_RE.fullmatch(run_name) is None:
        raise ProjectReadError('Invalid candidate workspace identifier')

    storage_fd = _open_owner_storage(storage)
    src_root_fd = None
    cand_root_fd = None
    created_here = False
    try:
        src_root_fd = _open_project_root(project_root)
        p_root = Path(project_root)
        s_root = Path(storage)
        if p_root == s_root or p_root.is_relative_to(s_root) or s_root.is_relative_to(p_root):
            raise ProjectReadError('Candidate storage and project root must not overlap')

        root_dev = os.fstat(src_root_fd).st_dev
        try:
            os.mkdir(run_name, 0o700, dir_fd=storage_fd)
        except FileExistsError as error:
            raise ProjectReadError('Candidate workspace identifier collision') from error
        created_here = True

        cand_root_fd = os.open(
            run_name,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
            dir_fd=storage_fd,
        )
        cand_st = os.fstat(cand_root_fd)
        if cand_st.st_uid != os.getuid() or stat.S_IMODE(cand_st.st_mode) != 0o700:
            raise ProjectReadError('Candidate workspace permissions invalid')

        state = {
            'run_name': run_name,
            'candidate_dir': s_root / run_name,
            'proposal_digest': digest,
            'visited': 0,
            'files_copied': 0,
            'dirs_created': 0,
            'total_bytes': 0,
            'skipped_hidden': 0,
            'skipped_extension': 0,
            'target_before_matched': False,
            'has_test_file': False,
        }

        def _walk_copy(src_fd, rel_parts, depth):
            _check_cancel(cancel)
            if depth > MAX_DIR_DEPTH:
                raise ProjectReadError('Project directory nesting exceeds limit')
            remaining = MAX_VISITED_ENTRIES - state['visited']
            names = _bounded_scandir_names(src_fd, remaining, cancel=cancel)
            state['visited'] += len(names)
            for name in names:
                _check_cancel(cancel)
                if not name or '/' in name or '\x00' in name:
                    raise ProjectReadError('Invalid filename in project snapshot')
                if name.startswith('.'):
                    state['skipped_hidden'] += 1
                    continue
                info = os.lstat(name, dir_fd=src_fd)
                if stat.S_ISLNK(info.st_mode):
                    raise ProjectReadError('Symlinks are not permitted in candidate snapshots')
                if info.st_dev != root_dev:
                    raise ProjectReadError('Cross-filesystem entries are not permitted')
                if stat.S_ISDIR(info.st_mode):
                    if depth + 1 > MAX_DIR_DEPTH:
                        raise ProjectReadError('Project directory nesting exceeds limit')
                    sub_fd = os.open(
                        name,
                        os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                        dir_fd=src_fd,
                    )
                    try:
                        sub_info = os.fstat(sub_fd)
                        if (
                            not stat.S_ISDIR(sub_info.st_mode)
                            or sub_info.st_dev != root_dev
                            or sub_info.st_ino != info.st_ino
                        ):
                            raise ProjectReadError('Project directory changed during snapshot')
                        _walk_copy(sub_fd, (*rel_parts, name), depth + 1)
                    finally:
                        os.close(sub_fd)
                    continue
                if not stat.S_ISREG(info.st_mode):
                    raise ProjectReadError('Non-regular files are not permitted in candidate snapshots')
                if info.st_nlink != 1:
                    raise ProjectReadError('Hardlinked files are not permitted in candidate snapshots')
                if Path(name).suffix not in ALLOWED_EXTENSIONS:
                    state['skipped_extension'] += 1
                    continue
                live_bytes = _read_regular_text_file(src_fd, name, info, root_dev, cancel=cancel)
                rel_path = '/'.join((*rel_parts, name))
                if rel_path == proposal.path:
                    if hashlib.sha256(live_bytes).hexdigest() != proposal.before_sha256:
                        raise ProjectReadError(
                            'Live target file no longer matches preview before_sha256'
                        )
                    write_bytes = after_bytes
                    state['target_before_matched'] = True
                else:
                    write_bytes = live_bytes
                state['files_copied'] += 1
                if state['files_copied'] > MAX_SNAPSHOT_FILES:
                    raise ProjectReadError('Project exceeds 256-file candidate snapshot cap')
                state['total_bytes'] += len(write_bytes)
                if state['total_bytes'] > MAX_TOTAL_BYTES:
                    raise ProjectReadError('Project exceeds 4 MiB candidate snapshot cap')
                _write_candidate_file(
                    cand_root_fd, rel_parts, name, write_bytes, state, cancel=cancel
                )
                if name.startswith('test_') and name.endswith('.py'):
                    state['has_test_file'] = True

        _walk_copy(src_root_fd, (), 0)
        if not state['target_before_matched']:
            raise ProjectReadError('Live target file was not found in eligible project snapshot')
        return state
    except BaseException:
        if cand_root_fd is not None:
            os.close(cand_root_fd)
            cand_root_fd = None
        if created_here:
            _cleanup_candidate_tree(storage_fd, run_name, best_effort=True)
        raise
    finally:
        if cand_root_fd is not None:
            os.close(cand_root_fd)
        if src_root_fd is not None:
            os.close(src_root_fd)
        os.close(storage_fd)


def _terminate(process):
    """Terminate an unwaited child's process group; never signals an already-reaped PID."""
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    if process.returncode is None:
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass


def _run_bounded_command(command, *, timeout_seconds=DEFAULT_TIMEOUT_SECONDS, cancel=None):
    """Synthetic bounded-output/timeout/cancel process utility (never runs project code)."""
    if (
        type(timeout_seconds) is bool
        or not isinstance(timeout_seconds, (int, float))
        or not math.isfinite(timeout_seconds)
        or not (MIN_TIMEOUT_SECONDS <= float(timeout_seconds) <= MAX_TIMEOUT_SECONDS)
    ):
        raise ProjectReadError('Invalid bounded command timeout')
    _check_cancel(cancel)

    process = None
    selector = None
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={},
            close_fds=True,
            start_new_session=True,
        )
        selector = selectors.DefaultSelector()
        combined = bytearray()
        output_exceeded = False
        timed_out = False
        canceled = False
        deadline = time.monotonic() + float(timeout_seconds)

        out_fd = process.stdout.fileno()
        err_fd = process.stderr.fileno()
        os.set_blocking(out_fd, False)
        os.set_blocking(err_fd, False)
        selector.register(out_fd, selectors.EVENT_READ, 'output')
        selector.register(err_fd, selectors.EVENT_READ, 'output')

        while selector.get_map() or process.poll() is None:
            if cancel is not None and cancel.is_set():
                canceled = True
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                break
            if selector.get_map():
                for key, _ in selector.select(timeout=min(0.05, remaining)):
                    fd = key.fd
                    cap = (MAX_OUTPUT_BYTES + 1) - len(combined)
                    try:
                        chunk = os.read(fd, min(4096, cap))
                    except BlockingIOError:
                        continue
                    except OSError:
                        selector.unregister(fd)
                        continue
                    if not chunk:
                        selector.unregister(fd)
                    else:
                        combined.extend(chunk)
                        if len(combined) > MAX_OUTPUT_BYTES:
                            del combined[MAX_OUTPUT_BYTES:]
                            output_exceeded = True
                            break
                if output_exceeded:
                    break
            else:
                time.sleep(min(0.02, remaining))

        if canceled:
            _terminate(process)
            raise ProjectReadError('Bounded command canceled')

        if timed_out or output_exceeded:
            _terminate(process)
            if process.returncode is None:
                raise ProjectReadError('Bounded command failed to terminate cleanly')
        elif process.returncode is None:
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                _terminate(process)
                raise ProjectReadError('Bounded command failed to exit cleanly')

        _check_cancel(cancel)
        raw = bytes(combined)
        return {
            'returncode': process.returncode,
            'timed_out': timed_out,
            'output_exceeded': output_exceeded,
            'output_raw': raw,
            'output': _sanitize_output(raw),
        }
    except BaseException:
        if process is not None:
            _terminate(process)
        raise
    finally:
        if selector is not None:
            selector.close()
        if process is not None:
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    try:
                        stream.close()
                    except OSError:
                        pass


def decode_candidate_test_frame(frame):
    """Decode one bounded candidate-test JSON frame. No roots or commands in JSON."""
    if type(frame) is not bytes or len(frame) > MAX_FRAME_BYTES or not frame.endswith(b'\n'):
        raise ProjectReadError('Invalid candidate-test frame')

    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError()
            result[key] = value
        return result

    try:
        value = json.loads(frame, object_pairs_hook=pairs)
        keys = {'schema', 'runner', 'whole_project_snapshot_approved', 'preview'}
        if (
            type(value) is not dict
            or set(value) != keys
            or value['schema'] != 'navi-candidate-test-request-v1'
            or value['runner'] != RUNNER_NAME
            or value['whole_project_snapshot_approved'] is not True
        ):
            raise ValueError()
        proposal = decode_preview_frame((json.dumps(value['preview']) + '\n').encode('utf-8'))
    except (ValueError, TypeError, UnicodeError, RecursionError) as error:
        raise ProjectReadError('Invalid candidate-test request') from error
    return proposal


def run_candidate_unittest(
    project_root,
    proposal,
    storage,
    *,
    whole_project_snapshot_approved=False,
    cancel=None,
    timeout_seconds=DEFAULT_TIMEOUT_SECONDS,
):
    """Unconditionally disabled pending exclusive cgroup launcher and owner approval route.

    Fails closed BEFORE reading project files or creating private candidate storage.
    """
    raise ProjectReadError(CANDIDATE_EXECUTION_UNAVAILABLE)


def candidate_test_request(project_root, storage, frame, *, cancel=None):
    """Unconditionally disabled pending exclusive cgroup launcher and owner approval route."""
    raise ProjectReadError(CANDIDATE_EXECUTION_UNAVAILABLE)


def main():
    """Unconditionally report candidate execution unavailable without reading project/storage."""
    print(
        json.dumps(
            {
                'error': CANDIDATE_EXECUTION_UNAVAILABLE,
                'status': 'not_implemented',
                'applied': False,
                'primary_project_untouched': True,
                'executed_project_code': False,
            },
            ensure_ascii=True,
        ),
        flush=True,
    )
    return 1


if __name__ == '__main__':
    raise SystemExit(main())