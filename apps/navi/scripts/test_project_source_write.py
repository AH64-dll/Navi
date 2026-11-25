"""Synthetic local verification tests for project_source_write.py.

Operates solely on synthetic temporary directories. Local tests are
verification, not proof of safety.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

WORKERS_DIR = Path(__file__).resolve().parent.parent / 'workers'
WORKER_SCRIPT = WORKERS_DIR / 'project_source_write.py'
sys.path.insert(0, str(WORKERS_DIR))

import project_source_write as psw


def _sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def _b64(data):
    return base64.b64encode(data).decode('ascii')


def _make_request(workspace_root, relative_path, before_bytes, new_bytes, **overrides):
    payload = {
        'schema': psw.SCHEMA,
        'workspace_root': str(workspace_root),
        'relative_path': relative_path,
        'expected_before_sha256': _sha256_bytes(before_bytes),
        'new_content_b64': _b64(new_bytes),
        'new_sha256': _sha256_bytes(new_bytes),
    }
    payload.update(overrides)
    return json.dumps(payload, ensure_ascii=True).encode('utf-8')


class _StatProxy:
    def __init__(self, real_st, **overrides):
        self._real = real_st
        self._overrides = overrides

    def __getattr__(self, name):
        return self._overrides[name] if name in self._overrides else getattr(self._real, name)


class ProjectSourceWriteTests(unittest.TestCase):
    def setUp(self):
        self.tmpdir = Path(tempfile.mkdtemp(prefix='navi-source-write-test-'))
        os.chmod(self.tmpdir, 0o700)
        self.workspace = self.tmpdir / 'workspace'
        self.workspace.mkdir(mode=0o700)
        self.outside = self.tmpdir / 'outside'
        self.outside.mkdir(mode=0o700)

    def tearDown(self):
        for current, dirs, _files in os.walk(self.tmpdir, topdown=True, followlinks=False):
            try:
                os.chmod(current, 0o700)
            except OSError:
                pass
            for dname in dirs:
                dpath = os.path.join(current, dname)
                if not os.path.islink(dpath):
                    try:
                        os.chmod(dpath, 0o700)
                    except OSError:
                        pass
        shutil.rmtree(self.tmpdir)

    def _assert_no_temp_files(self, root=None):
        scan = self.workspace if root is None else root
        for current, _dirs, files in os.walk(scan, followlinks=False):
            for fname in files:
                self.assertFalse(
                    fname.startswith('.navi-write-'),
                    f'leftover temp file in {current}: {fname}',
                )

    def _run_cli(self, raw_stdin, argv=()):
        return subprocess.run(
            [sys.executable, '-I', str(WORKER_SCRIPT), *argv],
            input=raw_stdin,
            capture_output=True,
            check=False,
            timeout=10,
        )

    def test_success_atomic_inode_change_and_no_path_or_content_echo(self):
        rel = 'src/nested/secret_target_name.py'
        target = self.workspace / rel
        target.parent.mkdir(parents=True, mode=0o755)
        before = b'SECRET_BEFORE_BODY = 1\n'
        after = b'SECRET_AFTER_BODY = 2\n'
        target.write_bytes(before)
        os.chmod(target, 0o640)
        before_ino = target.stat().st_ino

        req = _make_request(self.workspace, rel, before, after)
        proc = self._run_cli(req)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(proc.stderr, b'')

        raw_out = proc.stdout.decode('ascii')
        reply = json.loads(raw_out)
        self.assertEqual(
            reply,
            {
                'schema': psw.SCHEMA,
                'written': True,
                'relative_path_digest': _sha256_bytes(rel.encode('utf-8')),
                'before_sha256': _sha256_bytes(before),
                'after_sha256': _sha256_bytes(after),
                'bytes': len(after),
            },
        )
        for forbidden in (
            str(self.tmpdir),
            str(self.workspace),
            'secret_target_name',
            'src/nested',
            'SECRET_BEFORE_BODY',
            'SECRET_AFTER_BODY',
        ):
            self.assertNotIn(forbidden, raw_out)

        after_st = target.stat()
        self.assertNotEqual(after_st.st_ino, before_ino)
        self.assertEqual(stat.S_IMODE(after_st.st_mode), 0o640)
        self.assertEqual(target.read_bytes(), after)
        self._assert_no_temp_files()

    def test_empty_and_binary_writes_and_root_level_file(self):
        target = self.workspace / 'bin.dat'
        before = b'\x00\xff\x01\xfe'
        after = b''
        target.write_bytes(before)
        os.chmod(target, 0o600)
        before_ino = target.stat().st_ino

        res = psw.process_request_bytes(_make_request(self.workspace, 'bin.dat', before, after))
        self.assertTrue(res['written'], res)
        self.assertEqual(res['bytes'], 0)
        self.assertEqual(target.read_bytes(), b'')
        self.assertNotEqual(target.stat().st_ino, before_ino)
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)
        self._assert_no_temp_files()

    def test_before_mismatch_leaves_file_untouched(self):
        target = self.workspace / 'cas.py'
        before = b'ACTUAL = 1\n'
        target.write_bytes(before)
        os.chmod(target, 0o600)
        before_st = target.stat()

        res = psw.process_request_bytes(
            _make_request(self.workspace, 'cas.py', b'STALE = 0\n', b'NEW = 2\n')
        )
        self.assertEqual(
            res,
            {'schema': psw.SCHEMA, 'written': False, 'reason': psw.REASON_BEFORE_MISMATCH},
        )
        self.assertEqual(target.read_bytes(), before)
        self.assertEqual(target.stat().st_ino, before_st.st_ino)
        self._assert_no_temp_files()

    def test_symlink_target_and_symlinked_parent_dir_refused(self):
        outside_file = self.outside / 'real.txt'
        before = b'OUTSIDE = 1\n'
        outside_file.write_bytes(before)
        os.chmod(outside_file, 0o600)

        # Symlink target
        sym_target = self.workspace / 'link.txt'
        os.symlink(outside_file, sym_target)
        res = psw.process_request_bytes(
            _make_request(self.workspace, 'link.txt', before, b'HACKED\n')
        )
        self.assertEqual(res['written'], False)
        self.assertEqual(res['reason'], psw.REASON_INVALID_ENTRY_TYPE)
        self.assertTrue(sym_target.is_symlink())
        self.assertEqual(outside_file.read_bytes(), before)

        # Symlinked parent directory
        outside_dir = self.outside / 'sub'
        outside_dir.mkdir(mode=0o700)
        nested_real = outside_dir / 'file.txt'
        nested_real.write_bytes(before)
        os.chmod(nested_real, 0o600)
        os.symlink(outside_dir, self.workspace / 'symdir')

        res_dir = psw.process_request_bytes(
            _make_request(self.workspace, 'symdir/file.txt', before, b'HACKED\n')
        )
        self.assertEqual(res_dir['written'], False)
        self.assertEqual(res_dir['reason'], psw.REASON_INVALID_ENTRY_TYPE)
        self.assertEqual(nested_real.read_bytes(), before)
        self._assert_no_temp_files()
        self._assert_no_temp_files(self.outside)

    def test_hardlinked_directory_fifo_and_wrong_uid_targets_refused(self):
        before = b'ORIG = 1\n'

        # Hardlinked target (st_nlink == 2)
        hard = self.workspace / 'hard.txt'
        hard.write_bytes(before)
        os.chmod(hard, 0o600)
        alias = self.outside / 'hard-alias.txt'
        os.link(hard, alias)
        res_hard = psw.process_request_bytes(
            _make_request(self.workspace, 'hard.txt', before, b'NEW = 2\n')
        )
        self.assertEqual(res_hard['written'], False)
        self.assertEqual(res_hard['reason'], psw.REASON_INVALID_PERMISSIONS)
        self.assertEqual(hard.read_bytes(), before)
        self.assertEqual(alias.read_bytes(), before)

        # Directory target
        subdir = self.workspace / 'adir'
        subdir.mkdir(mode=0o700)
        res_dir = psw.process_request_bytes(
            _make_request(self.workspace, 'adir', b'', b'x')
        )
        self.assertEqual(res_dir['written'], False)
        self.assertEqual(res_dir['reason'], psw.REASON_INVALID_ENTRY_TYPE)
        self.assertTrue(subdir.is_dir())

        # FIFO target (must not block)
        fifo = self.workspace / 'pipe.txt'
        os.mkfifo(fifo, 0o600)
        res_fifo = psw.process_request_bytes(
            _make_request(self.workspace, 'pipe.txt', b'', b'x')
        )
        self.assertEqual(res_fifo['written'], False)
        self.assertEqual(res_fifo['reason'], psw.REASON_INVALID_ENTRY_TYPE)

        # Non-owner target (simulated wrong st_uid)
        other_uid = self.workspace / 'other_uid.txt'
        other_uid.write_bytes(before)
        os.chmod(other_uid, 0o600)
        real_lstat = os.lstat

        def fake_lstat(path, *args, **kwargs):
            st = real_lstat(path, *args, **kwargs)
            if path == 'other_uid.txt' and kwargs.get('dir_fd') is not None:
                return _StatProxy(st, st_uid=os.getuid() + 1)
            return st

        with mock.patch.object(os, 'lstat', side_effect=fake_lstat):
            res_uid = psw.process_request_bytes(
                _make_request(self.workspace, 'other_uid.txt', before, b'NEW\n')
            )
        self.assertEqual(res_uid['written'], False)
        self.assertEqual(res_uid['reason'], psw.REASON_INVALID_PERMISSIONS)
        self.assertEqual(other_uid.read_bytes(), before)
        self._assert_no_temp_files()

    def test_oversized_target_and_oversized_new_content_and_oversized_stdin(self):
        big_target = self.workspace / 'big.bin'
        big_bytes = b'A' * (psw.MAX_FILE_BYTES + 1)
        big_target.write_bytes(big_bytes)
        os.chmod(big_target, 0o600)
        res_big = psw.process_request_bytes(
            _make_request(self.workspace, 'big.bin', big_bytes, b'small')
        )
        self.assertEqual(res_big['written'], False)
        self.assertEqual(res_big['reason'], psw.REASON_TOO_LARGE)
        self.assertEqual(big_target.stat().st_size, psw.MAX_FILE_BYTES + 1)

        # Oversized new_bytes via direct API (> 262144 decoded bytes)
        small = self.workspace / 'small.bin'
        small.write_bytes(b'ok')
        os.chmod(small, 0o600)
        too_big_new = b'B' * (psw.MAX_FILE_BYTES + 1)
        res_new_big = psw.write_source_file(
            str(self.workspace),
            'small.bin',
            _sha256_bytes(b'ok'),
            too_big_new,
            _sha256_bytes(too_big_new),
        )
        self.assertEqual(res_new_big['written'], False)
        self.assertEqual(res_new_big['reason'], psw.REASON_TOO_LARGE)
        self.assertEqual(small.read_bytes(), b'ok')

        # Oversized stdin request (> 65536 bytes)
        big_stdin = b' ' * (psw.MAX_REQUEST_BYTES + 1)
        proc = self._run_cli(big_stdin)
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(
            json.loads(proc.stdout.decode('ascii')),
            {'schema': psw.SCHEMA, 'written': False, 'reason': psw.REASON_INVALID_REQUEST},
        )
        self._assert_no_temp_files()

    def test_wrong_new_sha256_bad_base64_duplicate_keys_extra_keys_and_bad_paths(self):
        target = self.workspace / 'strict.py'
        before = b'KEEP = 1\n'
        target.write_bytes(before)
        os.chmod(target, 0o600)
        before_ino = target.stat().st_ino

        # Wrong new_sha256 (verified before touching anything)
        res_sha = psw.process_request_bytes(
            _make_request(
                self.workspace,
                'strict.py',
                before,
                b'NEW = 2\n',
                new_sha256='0' * 64,
            )
        )
        self.assertEqual(res_sha['written'], False)
        self.assertEqual(res_sha['reason'], psw.REASON_INVALID_REQUEST)

        # Bad base64
        for bad_b64 in ('!!!not-base64!!!', 'ABC', 'SGVsbG8=\n', 'QQ='):
            res_b64 = psw.process_request_bytes(
                _make_request(
                    self.workspace,
                    'strict.py',
                    before,
                    b'NEW = 2\n',
                    new_content_b64=bad_b64,
                )
            )
            self.assertEqual(res_b64['written'], False, bad_b64)
            self.assertEqual(res_b64['reason'], psw.REASON_INVALID_REQUEST)

        # Duplicate JSON keys
        dup_json = (
            '{"schema":"navi-source-write-v1","schema":"navi-source-write-v1",'
            f'"workspace_root":{json.dumps(str(self.workspace))},'
            '"relative_path":"strict.py",'
            f'"expected_before_sha256":"{_sha256_bytes(before)}",'
            f'"new_content_b64":"{_b64(b"NEW = 2")}",'
            f'"new_sha256":"{_sha256_bytes(b"NEW = 2")}"}}'
        ).encode('utf-8')
        res_dup = psw.process_request_bytes(dup_json)
        self.assertEqual(res_dup['written'], False)
        self.assertEqual(res_dup['reason'], psw.REASON_INVALID_REQUEST)

        # Extra key
        res_extra = psw.process_request_bytes(
            _make_request(self.workspace, 'strict.py', before, b'NEW = 2\n', extra=True)
        )
        self.assertEqual(res_extra['written'], False)
        self.assertEqual(res_extra['reason'], psw.REASON_INVALID_REQUEST)

        # Bad relative paths: .., absolute, empty parts, NUL, >512 bytes, >16 parts
        bad_paths = [
            '',
            '/strict.py',
            '../strict.py',
            'a/../strict.py',
            'a/..',
            'a//b.py',
            'a/',
            './strict.py',
            'a\x00b.py',
            'a' * 513,
            '/'.join(f'p{i}' for i in range(17)),
        ]
        for bad_rel in bad_paths:
            res_path = psw.process_request_bytes(
                _make_request(self.workspace, bad_rel, before, b'NEW = 2\n')
            )
            self.assertEqual(res_path['written'], False, repr(bad_rel))
            self.assertEqual(res_path['reason'], psw.REASON_INVALID_REQUEST)

        # Symlinked / missing workspace_root -> invalid_storage
        sym_root = self.tmpdir / 'sym-workspace'
        os.symlink(self.workspace, sym_root)
        res_root = psw.process_request_bytes(
            _make_request(sym_root, 'strict.py', before, b'NEW = 2\n')
        )
        self.assertEqual(res_root['written'], False)
        self.assertEqual(res_root['reason'], psw.REASON_INVALID_STORAGE)

        # Unexpected CLI args rejected
        proc_argv = self._run_cli(
            _make_request(self.workspace, 'strict.py', before, b'NEW = 2\n'),
            argv=('--extra',),
        )
        self.assertEqual(proc_argv.returncode, 1)
        self.assertEqual(
            json.loads(proc_argv.stdout.decode('ascii')),
            {'schema': psw.SCHEMA, 'written': False, 'reason': psw.REASON_INVALID_REQUEST},
        )

        self.assertEqual(target.read_bytes(), before)
        self.assertEqual(target.stat().st_ino, before_ino)
        self._assert_no_temp_files()

    def test_mode_preserved_and_never_widened(self):
        real_open = os.open
        for target_mode in (0o600, 0o640, 0o644, 0o755, 0o400):
            fname = f'mode_{oct(target_mode)}.txt'
            target = self.workspace / fname
            before = b'before\n'
            after = b'after\n'
            target.write_bytes(before)
            os.chmod(target, target_mode)

            seen_create_modes = []

            def spy_open(path, flags, mode=0o777, *, dir_fd=None):
                if flags & os.O_CREAT:
                    seen_create_modes.append(mode)
                return real_open(path, flags, mode, dir_fd=dir_fd)

            old_umask = os.umask(0o022)
            try:
                with mock.patch.object(os, 'open', side_effect=spy_open):
                    res = psw.process_request_bytes(
                        _make_request(self.workspace, fname, before, after)
                    )
            finally:
                os.umask(old_umask)

            self.assertTrue(res['written'], (oct(target_mode), res))
            self.assertEqual(seen_create_modes, [target_mode])
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), target_mode)
            self.assertEqual(target.read_bytes(), after)
            self._assert_no_temp_files()

    def test_no_temp_file_left_behind_on_failure_cancel_and_races(self):
        before = b'SAFE_CONTENT = 1\n'
        after = b'NEXT_CONTENT = 2\n'

        # 1. Failure during write cleans up temp file
        t1 = self.workspace / 'fail_write.py'
        t1.write_bytes(before)
        os.chmod(t1, 0o600)
        with mock.patch.object(os, 'write', side_effect=OSError('disk full')):
            res1 = psw.process_request_bytes(_make_request(self.workspace, 'fail_write.py', before, after))
        self.assertEqual(res1['written'], False)
        self.assertEqual(res1['reason'], psw.REASON_VERIFICATION_FAILED)
        self.assertEqual(t1.read_bytes(), before)
        self._assert_no_temp_files()

        # 2. Failure during rename cleans up temp file
        t2 = self.workspace / 'fail_rename.py'
        t2.write_bytes(before)
        os.chmod(t2, 0o600)
        with mock.patch.object(os, 'rename', side_effect=OSError('rename failed')):
            res2 = psw.process_request_bytes(_make_request(self.workspace, 'fail_rename.py', before, after))
        self.assertEqual(res2['written'], False)
        self.assertEqual(res2['reason'], psw.REASON_VERIFICATION_FAILED)
        self.assertEqual(t2.read_bytes(), before)
        self._assert_no_temp_files()

        # 3. Cancel mid-write cleans up temp file and leaves target untouched
        t3 = self.workspace / 'cancel_mid.py'
        t3.write_bytes(before)
        os.chmod(t3, 0o600)
        cancel = threading.Event()
        real_write = os.write

        def cancel_on_write(fd, buf):
            cancel.set()
            return real_write(fd, buf)

        with mock.patch.object(os, 'write', side_effect=cancel_on_write):
            res3 = psw.process_request_bytes(
                _make_request(self.workspace, 'cancel_mid.py', before, after),
                cancel=cancel,
            )
        self.assertEqual(res3['written'], False)
        self.assertEqual(res3['reason'], psw.REASON_VERIFICATION_FAILED)
        self.assertEqual(t3.read_bytes(), before)
        self._assert_no_temp_files()

        # 4. Expired deadline fails without touching target
        t4 = self.workspace / 'timeout.py'
        t4.write_bytes(before)
        os.chmod(t4, 0o600)
        res4 = psw.process_request_bytes(
            _make_request(self.workspace, 'timeout.py', before, after),
            deadline=time.monotonic() - 1.0,
        )
        self.assertEqual(res4['written'], False)
        self.assertEqual(res4['reason'], psw.REASON_VERIFICATION_FAILED)
        self.assertEqual(t4.read_bytes(), before)
        self._assert_no_temp_files()

        # 5. Post-rename race replacing file with a different inode -> directory_changed
        t5 = self.workspace / 'race_post_rename.py'
        t5.write_bytes(before)
        os.chmod(t5, 0o600)
        real_rename = os.rename

        def swap_right_after_rename(src, dst, *, src_dir_fd=None, dst_dir_fd=None):
            real_rename(src, dst, src_dir_fd=src_dir_fd, dst_dir_fd=dst_dir_fd)
            intruder = self.workspace / 'intruder.tmp'
            intruder.write_bytes(after)
            os.chmod(intruder, 0o600)
            real_rename(intruder, t5)

        with mock.patch.object(os, 'rename', side_effect=swap_right_after_rename):
            res5 = psw.process_request_bytes(
                _make_request(self.workspace, 'race_post_rename.py', before, after)
            )
        self.assertEqual(res5['written'], False)
        self.assertEqual(res5['reason'], psw.REASON_DIRECTORY_CHANGED)
        self.assertTrue(t5.is_file())
        self._assert_no_temp_files()

        # 6. Post-rename verification mismatch -> verification_failed, target not deleted
        t6 = self.workspace / 'verify_fail.py'
        t6.write_bytes(before)
        os.chmod(t6, 0o600)
        real_read_chunked = psw._read_pinned_fd_chunked
        read_calls = [0]

        def corrupt_second_read(fd, st, *, cancel, deadline):
            read_calls[0] += 1
            data = real_read_chunked(fd, st, cancel=cancel, deadline=deadline)
            if read_calls[0] == 2:
                return b'X' * len(data)
            return data

        with mock.patch.object(psw, '_read_pinned_fd_chunked', side_effect=corrupt_second_read):
            res6 = psw.process_request_bytes(
                _make_request(self.workspace, 'verify_fail.py', before, after)
            )
        self.assertEqual(res6['written'], False)
        self.assertEqual(res6['reason'], psw.REASON_VERIFICATION_FAILED)
        self.assertTrue(t6.is_file())
        self._assert_no_temp_files()

        # 7. Missing file -> not_found, no temp file created
        res7 = psw.process_request_bytes(
            _make_request(self.workspace, 'does/not/exist.py', before, after)
        )
        self.assertEqual(res7['written'], False)
        self.assertEqual(res7['reason'], psw.REASON_NOT_FOUND)
        self._assert_no_temp_files()


if __name__ == '__main__':
    unittest.main()
