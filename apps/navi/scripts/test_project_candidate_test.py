"""Synthetic local verification tests for preparatory project_candidate_test.py.

Aggregate sandbox execution is NOT_IMPLEMENTED and remains unconditionally
disabled. These synthetic tests verify fail-closed execution refusal, bounded
snapshot/cleanup invariants, frame decoding, and bounded output/process helpers.
Local tests are verification, not proof of safety.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

WORKERS_DIR = Path(__file__).resolve().parent.parent / 'workers'
sys.path.insert(0, str(WORKERS_DIR))

import project_candidate_test as pct
from project_edits import ProjectReadError, propose_edit


def _sha256(text):
    return hashlib.sha256(text.encode('utf-8')).hexdigest()


def _make_proposal(path, before, after):
    return propose_edit(path, _sha256(before), before, after)


def _make_preview_dict(proposal):
    return {
        'schema': 'navi-edit-preview-v1',
        'applied': False,
        'requires_fresh_validation': True,
        'path': proposal.path,
        'before_sha256': proposal.before_sha256,
        'after_sha256': proposal.after_sha256,
        'before': proposal.before,
        'after': proposal.after,
    }


def _make_request_frame(proposal, *, runner='python-unittest', approved=True):
    payload = {
        'schema': 'navi-candidate-test-request-v1',
        'runner': runner,
        'whole_project_snapshot_approved': approved,
        'preview': _make_preview_dict(proposal),
    }
    return (json.dumps(payload) + '\n').encode('utf-8')


def _has_safe_owner_ancestors(candidate):
    try:
        path = Path(candidate).resolve()
        if not path.is_absolute() or len(path.parts) <= 1 or not path.is_dir():
            return False
        fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            for part in path.parts[1:]:
                nxt = os.open(
                    part,
                    os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                    dir_fd=fd,
                )
                os.close(fd)
                fd = nxt
                st = os.fstat(fd)
                if st.st_mode & 0o022 or st.st_uid not in (0, os.getuid()):
                    return False
            return os.access(path, os.W_OK | os.X_OK)
        finally:
            os.close(fd)
    except OSError:
        return False


def _find_safe_test_parent():
    for candidate in (Path.home(), Path(__file__).resolve().parent, Path.cwd()):
        if _has_safe_owner_ancestors(candidate):
            return Path(candidate).resolve()
    raise RuntimeError(
        'No owner-safe non-group/world-writable parent directory available for private storage tests'
    )


class CandidateTestCaseBase(unittest.TestCase):
    def setUp(self):
        parent = _find_safe_test_parent()
        self.base = Path(tempfile.mkdtemp(prefix='.navi-cand-test-', dir=str(parent)))
        os.chmod(self.base, 0o700)
        self.data_root = self.base / 'owner-data'
        self.data_root.mkdir(mode=0o700)
        os.chmod(self.data_root, 0o700)
        self.storage = pct.owner_candidate_storage(self.data_root)
        self.project = self.base / 'project'
        self.project.mkdir(mode=0o700)

    def tearDown(self):
        shutil.rmtree(self.base, ignore_errors=True)

    def _write(self, rel, text):
        target = self.project / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding='utf-8')
        return target

    def _make_basic_project(self):
        before = 'def add(a, b):\n    return a - b\n'
        after = 'def add(a, b):\n    return a + b\n'
        self._write('calc.py', before)
        self._write(
            'test_calc.py',
            'import unittest\n'
            'import calc\n'
            'class CalcTests(unittest.TestCase):\n'
            '    def test_add(self):\n'
            '        self.assertEqual(calc.add(2, 3), 5)\n',
        )
        return before, after, _make_proposal('calc.py', before, after)


class TestUnconditionalExecutionRefusal(CandidateTestCaseBase):
    def test_aggregate_sandbox_marked_not_implemented(self):
        self.assertEqual(pct.AGGREGATE_SANDBOX_STATUS, 'NOT_IMPLEMENTED')

    def test_run_candidate_unittest_refuses_even_with_approval_before_any_io(self):
        _, _, proposal = self._make_basic_project()
        with mock.patch.object(
            pct, '_open_project_root', side_effect=AssertionError('must not read project')
        ), mock.patch.object(
            pct, '_open_owner_storage', side_effect=AssertionError('must not touch storage')
        ), mock.patch.object(
            pct, '_build_candidate_snapshot', side_effect=AssertionError('must not snapshot')
        ):
            for approved in (True, False, None):
                with self.assertRaises(ProjectReadError) as ctx:
                    pct.run_candidate_unittest(
                        self.project,
                        proposal,
                        self.storage,
                        whole_project_snapshot_approved=approved,
                    )
                self.assertIn('not implemented', str(ctx.exception).lower())
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_candidate_test_request_refuses_before_project_reads_or_snapshot(self):
        _, _, proposal = self._make_basic_project()
        frame = _make_request_frame(proposal, approved=True)
        with mock.patch.object(
            pct, '_open_project_root', side_effect=AssertionError('must not read project')
        ), mock.patch.object(
            pct, '_build_candidate_snapshot', side_effect=AssertionError('must not snapshot')
        ):
            with self.assertRaises(ProjectReadError):
                pct.candidate_test_request(self.project, self.storage, frame)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_cli_unconditionally_refuses_without_creating_storage_or_env_bypass(self):
        _, _, proposal = self._make_basic_project()
        fresh_data = self.base / 'untouched-owner-data'
        fresh_data.mkdir(mode=0o700)
        os.chmod(fresh_data, 0o700)
        env = {'NAVI_PROJECT_CANDIDATE_TEST': '1'}
        proc = subprocess.run(
            [
                sys.executable,
                '-I',
                str(WORKERS_DIR / 'project_candidate_test.py'),
                '--owner-data',
                str(fresh_data),
                '--project-root',
                str(self.project),
            ],
            input=_make_request_frame(proposal, approved=True),
            capture_output=True,
            env=env,
            timeout=5,
        )
        self.assertEqual(proc.returncode, 1)
        payload = json.loads(proc.stdout.decode('utf-8'))
        self.assertEqual(payload['status'], 'not_implemented')
        self.assertFalse(payload['applied'])
        self.assertTrue(payload['primary_project_untouched'])
        self.assertFalse(payload['executed_project_code'])
        # Must not create candidate-runs/ when execution is unavailable.
        self.assertEqual(list(fresh_data.iterdir()), [])


class TestFrameDecoder(CandidateTestCaseBase):
    def test_decodes_valid_frame_and_rejects_unapproved_wrong_runner_extra_or_dup_keys(self):
        _, _, proposal = self._make_basic_project()
        self.assertEqual(pct.decode_candidate_test_frame(_make_request_frame(proposal)), proposal)

        for bad_approved in (False, None, 1, 'true'):
            with self.assertRaises(ProjectReadError):
                pct.decode_candidate_test_frame(
                    _make_request_frame(proposal, approved=bad_approved)
                )
        for bad_runner in ('node-test', 'pytest', 'sh', ''):
            with self.assertRaises(ProjectReadError):
                pct.decode_candidate_test_frame(_make_request_frame(proposal, runner=bad_runner))

        # Extra key rejected
        extra = json.loads(_make_request_frame(proposal).decode('utf-8'))
        extra['command'] = ['sh']
        with self.assertRaises(ProjectReadError):
            pct.decode_candidate_test_frame((json.dumps(extra) + '\n').encode('utf-8'))

        # Duplicate JSON key rejected
        raw_dup = (
            '{"schema":"navi-candidate-test-request-v1","runner":"python-unittest",'
            '"whole_project_snapshot_approved":false,"whole_project_snapshot_approved":true,'
            '"preview":' + json.dumps(_make_preview_dict(proposal)) + '}\n'
        ).encode('utf-8')
        with self.assertRaises(ProjectReadError):
            pct.decode_candidate_test_frame(raw_dup)

        # Missing newline rejected
        with self.assertRaises(ProjectReadError):
            pct.decode_candidate_test_frame(_make_request_frame(proposal).rstrip(b'\n'))


class TestPreparatorySnapshotAndCleanup(CandidateTestCaseBase):
    def _build(self, proposal, *, run_name=None, approved=True, cancel=None):
        return pct._build_candidate_snapshot(
            self.project,
            proposal,
            self.storage,
            run_name=run_name,
            whole_project_snapshot_approved=approved,
            cancel=cancel,
        )

    def test_requires_explicit_bool_precondition(self):
        _, _, proposal = self._make_basic_project()
        for bad_flag in (False, None, 1, 'true'):
            with self.assertRaises(ProjectReadError):
                self._build(proposal, approved=bad_flag)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_private_only_overlay_filtering_and_complete_cleanup(self):
        before, after, proposal = self._make_basic_project()
        self._write('config.json', '{"ok": true}\n')
        self._write('settings.toml', 'flag = true\n')
        self._write('notes.txt', 'hello\n')
        self._write('README.md', '# skipped\n')
        self._write('.env', 'SECRET=do-not-copy\n')
        self._write('.git/config', '[core]\n')
        self._write('.hidden/secret.py', 'TOKEN = "x"\n')
        self._write('__pycache__/calc.cpython-310.pyc', 'bytecode')

        state = self._build(proposal)
        cand = state['candidate_dir']
        self.assertEqual(state['files_copied'], 5)
        self.assertEqual(state['skipped_hidden'], 3)
        self.assertEqual(state['skipped_extension'], 2)
        self.assertTrue(state['has_test_file'])
        self.assertTrue(state['target_before_matched'])
        self.assertEqual((cand / 'calc.py').read_text(encoding='utf-8'), after)
        self.assertEqual((self.project / 'calc.py').read_text(encoding='utf-8'), before)
        self.assertFalse((cand / '.env').exists())
        self.assertFalse((cand / '.git').exists())
        self.assertFalse((cand / '.hidden').exists())
        self.assertFalse((cand / 'README.md').exists())
        self.assertFalse((cand / '__pycache__').exists())

        pct._cleanup_candidate_workspace(self.storage, state['run_name'], best_effort=False)
        self.assertEqual(list(self.storage.iterdir()), [])
        self.assertEqual((self.project / 'calc.py').read_text(encoding='utf-8'), before)

    def test_rejects_hidden_or_ineligible_proposal_target(self):
        # Earliest refusal: shared propose_edit rejects hidden paths directly.
        self._write('.secret.py', 'a = 1\n')
        with self.assertRaises(ProjectReadError):
            _make_proposal('.secret.py', 'a = 1\n', 'a = 2\n')
        self._write('sub/.hidden.py', 'a = 1\n')
        with self.assertRaises(ProjectReadError):
            _make_proposal('sub/.hidden.py', 'a = 1\n', 'a = 2\n')

        # Eligible for propose_edit, refused by candidate snapshot allowlist.
        self._write('README.md', 'old\n')
        p_md = _make_proposal('README.md', 'old\n', 'new\n')
        with self.assertRaises(ProjectReadError):
            self._build(p_md)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_rejects_invalid_utf8_and_nul_bytes_in_eligible_extensions(self):
        _, _, proposal = self._make_basic_project()
        (self.project / 'bad_utf8.txt').write_bytes(b'hello \xff\xfe world\n')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])
        (self.project / 'bad_utf8.txt').unlink()

        (self.project / 'binary_disguised.py').write_bytes(b'x = 1\x00\n')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_rejects_symlink_hardlink_and_fifo_with_cleanup(self):
        _, _, proposal = self._make_basic_project()
        outside = self.base / 'outside.py'
        outside.write_text('X = 1\n', encoding='utf-8')
        os.symlink(outside, self.project / 'link.py')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])
        os.unlink(self.project / 'link.py')

        outside_dir = self.base / 'outside_dir'
        outside_dir.mkdir(mode=0o700)
        os.symlink(outside_dir, self.project / 'sublink')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])
        os.unlink(self.project / 'sublink')

        other = self.base / 'hard_target.py'
        other.write_text('Y = 2\n', encoding='utf-8')
        os.link(other, self.project / 'linked.py')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])
        os.unlink(self.project / 'linked.py')

        os.mkfifo(self.project / 'pipe.txt')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_rejects_stale_before_sha256_and_missing_target(self):
        _, _, proposal = self._make_basic_project()
        self._write('calc.py', 'def add(a, b):\n    return a * b\n')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])

        (self.project / 'calc.py').unlink()
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_enforces_256_file_64kib_file_and_4mib_total_caps(self):
        _, _, proposal = self._make_basic_project()
        # 256 files exact passes, 257 fails
        for idx in range(254):
            self._write(f'm_{idx:03d}.txt', 'ok\n')
        state = self._build(proposal)
        self.assertEqual(state['files_copied'], 256)
        pct._cleanup_candidate_workspace(self.storage, state['run_name'], best_effort=False)
        self.assertEqual(list(self.storage.iterdir()), [])

        self._write('m_254.txt', 'too many\n')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])
        for idx in range(255):
            (self.project / f'm_{idx:03d}.txt').unlink()

        # 64 KiB exact passes, 64 KiB + 1 fails
        self._write('exact.txt', 'a' * pct.MAX_FILE_BYTES)
        state = self._build(proposal)
        pct._cleanup_candidate_workspace(self.storage, state['run_name'], best_effort=False)
        self._write('oversized.txt', 'a' * (pct.MAX_FILE_BYTES + 1))
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])
        (self.project / 'exact.txt').unlink()
        (self.project / 'oversized.txt').unlink()

        # 4 MiB total cap (64 * 64 KiB + calc.py + test_calc.py > 4 MiB)
        chunk = 'x' * pct.MAX_FILE_BYTES
        for idx in range(64):
            self._write(f'big_{idx:02d}.txt', chunk)
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_bounded_scandir_stops_at_cap_without_exhausting_directory(self):
        _, _, proposal = self._make_basic_project()
        for idx in range(40):
            self._write(f'extra_{idx:02d}.md', 'x\n')
        seen = [0]
        real_scandir = pct.os.scandir

        class CountingScandir:
            def __init__(self, fd):
                self._inner = real_scandir(fd)
            def __enter__(self):
                self._it = self._inner.__enter__()
                return self
            def __exit__(self, *args):
                return self._inner.__exit__(*args)
            def __iter__(self):
                return self
            def __next__(self):
                item = next(self._it)
                seen[0] += 1
                return item

        with mock.patch.object(pct, 'MAX_VISITED_ENTRIES', 10), mock.patch.object(
            pct.os, 'scandir', CountingScandir
        ):
            with self.assertRaises(ProjectReadError):
                self._build(proposal)
        self.assertEqual(seen[0], 11)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_directory_cap_and_cleanup_budget_consistency(self):
        _, _, proposal = self._make_basic_project()
        # 16 branches * 16 levels = 256 created dirs (exact cap) + 18 files
        for branch in range(16):
            rel = '/'.join([f'b{branch:02d}'] + [f'd{lvl:02d}' for lvl in range(15)] + ['f.txt'])
            self._write(rel, 'ok\n')
        state = self._build(proposal)
        self.assertEqual(state['dirs_created'], pct.MAX_SNAPSHOT_DIRS)
        pct._cleanup_candidate_workspace(self.storage, state['run_name'], best_effort=False)
        self.assertEqual(list(self.storage.iterdir()), [])

        # Directory #257 fails closed before creation and cleans up completely
        self._write('overflow_dir/g.txt', 'too many dirs\n')
        with self.assertRaises(ProjectReadError):
            self._build(proposal)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_forced_token_collision_preserves_existing_run_bytes(self):
        _, _, proposal = self._make_basic_project()
        fixed_hex = 'c' * 32
        existing_dir = self.storage / ('run-' + fixed_hex)
        existing_dir.mkdir(mode=0o700)
        os.chmod(existing_dir, 0o700)
        sentinel = existing_dir / 'keep.txt'
        sentinel_bytes = b'existing-legitimate-run-do-not-delete\n'
        sentinel.write_bytes(sentinel_bytes)

        with mock.patch.object(pct.secrets, 'token_hex', return_value=fixed_hex):
            with self.assertRaises(ProjectReadError):
                self._build(proposal)
        self.assertTrue(existing_dir.is_dir())
        self.assertEqual(sentinel.read_bytes(), sentinel_bytes)

    def test_cancel_during_snapshot_cleans_up_created_run(self):
        _, _, proposal = self._make_basic_project()
        # Pre-set cancel
        pre_cancel = threading.Event()
        pre_cancel.set()
        with self.assertRaises(ProjectReadError):
            self._build(proposal, cancel=pre_cancel)
        self.assertEqual(list(self.storage.iterdir()), [])

        # Mid-snapshot cancel triggered after candidate dir creation
        mid_cancel = threading.Event()
        real_read = pct._read_regular_text_file

        def cancel_on_read(*args, **kwargs):
            mid_cancel.set()
            return real_read(*args, **kwargs)

        with mock.patch.object(pct, '_read_regular_text_file', side_effect=cancel_on_read):
            with self.assertRaises(ProjectReadError):
                self._build(proposal, cancel=mid_cancel)
        self.assertEqual(list(self.storage.iterdir()), [])

    def test_rejects_overlapping_storage_and_unsafe_storage_perms(self):
        _, _, proposal = self._make_basic_project()
        nested = self.project / 'nested-storage'
        nested.mkdir(mode=0o700)
        os.chmod(nested, 0o700)
        with self.assertRaises(ProjectReadError):
            pct._build_candidate_snapshot(
                self.project, proposal, nested, whole_project_snapshot_approved=True
            )

        unsafe = self.base / 'world-readable'
        unsafe.mkdir(mode=0o755)
        os.chmod(unsafe, 0o755)
        with self.assertRaises(ProjectReadError):
            pct._build_candidate_snapshot(
                self.project, proposal, unsafe, whole_project_snapshot_approved=True
            )

    def test_bounded_cleanup_refuses_symlinks_without_touching_outside_target(self):
        run_name = 'run-' + ('b' * 32)
        outside = self.base / 'keep.txt'
        outside.write_text('keep\n', encoding='utf-8')
        run_dir = self.storage / run_name
        run_dir.mkdir(mode=0o700)
        (run_dir / 'regular.txt').write_text('bye\n', encoding='utf-8')
        os.symlink(outside, run_dir / 'trap')

        with self.assertRaises(ProjectReadError):
            pct._cleanup_candidate_workspace(self.storage, run_name, best_effort=False)
        self.assertTrue(outside.exists())
        self.assertFalse((run_dir / 'regular.txt').exists())
        os.unlink(run_dir / 'trap')
        pct._cleanup_candidate_workspace(self.storage, run_name, best_effort=False)
        self.assertFalse(run_dir.exists())


class TestSyntheticBoundedUtilities(CandidateTestCaseBase):
    def test_nonblocking_combined_32kib_output_cap_and_sanitization(self):
        script = (
            'import sys\n'
            'sys.stdout.write("\\x1b[31m\\u202e" + ("A" * 20000))\n'
            'sys.stdout.flush()\n'
            'sys.stderr.write("B" * 25000)\n'
            'sys.stderr.flush()\n'
        )
        outcome = pct._run_bounded_command(
            [sys.executable, '-E', '-s', '-c', script],
            timeout_seconds=5.0,
        )
        self.assertTrue(outcome['output_exceeded'])
        self.assertEqual(len(outcome['output_raw']), pct.MAX_OUTPUT_BYTES)
        self.assertNotIn('\x1b', outcome['output'])
        self.assertNotIn('\u202e', outcome['output'])

    def test_timeout_and_cancel_reap_process_group(self):
        sleep_cmd = [sys.executable, '-E', '-s', '-c', 'import time; time.sleep(30)']
        start = time.monotonic()
        outcome = pct._run_bounded_command(sleep_cmd, timeout_seconds=0.5)
        self.assertTrue(outcome['timed_out'])
        self.assertLess(time.monotonic() - start, 4.0)

        cancel = threading.Event()
        timer = threading.Timer(0.2, cancel.set)
        timer.start()
        try:
            with self.assertRaises(ProjectReadError):
                pct._run_bounded_command(sleep_cmd, timeout_seconds=5.0, cancel=cancel)
        finally:
            timer.cancel()

    def test_terminate_never_signals_already_reaped_pid(self):
        proc = subprocess.Popen(
            [sys.executable, '-E', '-s', '-c', 'pass'],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        proc.wait(timeout=5)
        self.assertIsNotNone(proc.returncode)
        with mock.patch.object(
            pct.os, 'killpg', side_effect=AssertionError('must not signal reaped pid')
        ):
            pct._terminate(proc)

    def test_popen_failure_does_not_leak_fds(self):
        fd_dir = Path('/proc/self/fd')
        before_count = len(os.listdir(fd_dir)) if fd_dir.is_dir() else None
        with self.assertRaises(OSError):
            pct._run_bounded_command(
                [str(self.base / 'nonexistent-binary')],
                timeout_seconds=2.0,
            )
        if before_count is not None:
            self.assertEqual(len(os.listdir(fd_dir)), before_count)


if __name__ == '__main__':
    unittest.main()