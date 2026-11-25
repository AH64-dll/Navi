"""Synthetic local verification tests for project_review_cleanup.py.

Operates solely on synthetic temporary directories. Local tests are
verification, not proof of safety.
"""
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
CLEANUP_SCRIPT = WORKERS_DIR / 'project_review_cleanup.py'
sys.path.insert(0, str(WORKERS_DIR))

from project_edit_reviews import proposal_digest
from project_edits import ProjectReadError, propose_edit
import project_review_cleanup as prc
import project_review_inventory as pri


def _sha256(text):
    return hashlib.sha256(text.encode('utf-8')).hexdigest()


def _make_proposal(path='src/app.py', before='VALUE = 1\n', after='VALUE = 2\n'):
    return propose_edit(path, _sha256(before), before, after)


def _make_multichunk_proposal(path='src/large.py'):
    lines = [f'LINE_{i:02d} = "{"x" * 64}"\n' for i in range(40)]
    before = ''.join(lines)
    after_lines = list(lines)
    after_lines[0] = f'LINE_00 = "{"y" * 64}"\n'
    return _make_proposal(path, before, ''.join(after_lines))


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
    raise RuntimeError('No owner-safe parent directory available for private storage tests')


def _write_review(parent, name, proposal):
    folder = parent / name
    folder.mkdir(mode=0o700)
    os.chmod(folder, 0o700)
    manifest = {
        'schema': 'navi-private-review-v1',
        'applied': False,
        'requires_fresh_validation': True,
        'original_path': proposal.path,
        'proposal_digest': proposal_digest(proposal),
        'before_sha256': proposal.before_sha256,
        'after_sha256': proposal.after_sha256,
        'notice': 'Review files only. No project file was changed. Do not execute these snapshots.',
    }
    files = (
        ('before.txt', proposal.before.encode('utf-8')),
        ('proposed-after.txt', proposal.after.encode('utf-8')),
        ('review.json', (json.dumps(manifest, ensure_ascii=True, indent=2) + '\n').encode('utf-8')),
    )
    for fname, raw in files:
        target = folder / fname
        target.write_bytes(raw)
        os.chmod(target, 0o600)
    return folder


def _make_frame(review_id, digest):
    body = {
        'schema': prc.REQUEST_SCHEMA,
        'review_id': review_id,
        'proposal_digest': digest,
    }
    return (json.dumps(body, separators=(',', ':'), ensure_ascii=True) + '\n').encode('ascii')


def _force_rmtree(root):
    if not root.exists():
        return
    for current, dirs, _files in os.walk(root, topdown=True, followlinks=False):
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
    shutil.rmtree(root)


class _StatProxy:
    def __init__(self, real_st, **overrides):
        self._real = real_st
        self._overrides = overrides

    def __getattr__(self, name):
        return self._overrides[name] if name in self._overrides else getattr(self._real, name)


class ReviewCleanupTests(unittest.TestCase):
    def setUp(self):
        parent = _find_safe_test_parent()
        self.tmpdir = Path(tempfile.mkdtemp(prefix='navi-review-cleanup-test-', dir=parent))
        os.chmod(self.tmpdir, 0o700)
        self.owner_data = self.tmpdir / 'owner-data'
        self.owner_data.mkdir(mode=0o700)
        os.chmod(self.owner_data, 0o700)
        self.storage = self.owner_data / pri.PROPOSAL_REVIEWS_DIR
        self.storage.mkdir(mode=0o700)
        os.chmod(self.storage, 0o700)
        self.project_dir = self.tmpdir / 'synthetic-project'
        self.project_dir.mkdir(mode=0o700)
        os.chmod(self.project_dir, 0o700)

    def tearDown(self):
        _force_rmtree(self.tmpdir)

    def _rid(self, ch):
        return 'review-' + ch * 32

    def _make_review(self, ch, proposal=None):
        prop = _make_proposal() if proposal is None else proposal
        rid = self._rid(ch)
        folder = _write_review(self.storage, rid, prop)
        return rid, proposal_digest(prop), folder

    def _assert_all_files_present(self, folder):
        for name in pri.FIXED_REVIEW_FILES:
            self.assertTrue((folder / name).is_file(), f'missing {name}')

    def _run_cli(self, argv, stdin_bytes):
        return subprocess.run(
            [sys.executable, str(CLEANUP_SCRIPT), *argv],
            input=stdin_bytes,
            capture_output=True,
            check=False,
            timeout=10,
        )

    def test_valid_cleanup_uses_pinned_fds_and_inventory_verification(self):
        rid, digest, _folder = self._make_review('a')
        inv = pri.list_review_inventory(self.owner_data)
        self.assertEqual(inv['valid_count'], 1)
        self.assertEqual(inv['items'][0]['proposal_digest'], digest)

        inspect_calls, unlink_calls, rmdir_calls = [], [], []
        real_inspect, real_unlink, real_rmdir = (
            pri.inspect_pinned_review_workspace,
            os.unlink,
            os.rmdir,
        )

        def spy_inspect(*args, **kwargs):
            inspect_calls.append((args, kwargs))
            return real_inspect(*args, **kwargs)

        def spy_unlink(name, *, dir_fd=None):
            unlink_calls.append((name, dir_fd))
            return real_unlink(name, dir_fd=dir_fd)

        def spy_rmdir(name, *, dir_fd=None):
            rmdir_calls.append((name, dir_fd))
            return real_rmdir(name, dir_fd=dir_fd)

        with (
            mock.patch.object(prc, 'inspect_pinned_review_workspace', side_effect=spy_inspect),
            mock.patch.object(os, 'unlink', side_effect=spy_unlink),
            mock.patch.object(os, 'rmdir', side_effect=spy_rmdir),
        ):
            res = prc.cleanup_review_workspace(self.owner_data, rid, digest)

        self.assertEqual(
            res,
            {'schema': prc.CLEANUP_SCHEMA, 'deleted': True, 'review_id': rid},
        )
        self.assertEqual(len(inspect_calls), 1)
        self.assertEqual([n for n, _fd in unlink_calls], list(pri.FIXED_REVIEW_FILES))
        self.assertTrue(all(isinstance(fd, int) and fd >= 0 for _n, fd in unlink_calls))
        self.assertEqual(len({fd for _n, fd in unlink_calls}), 1)
        self.assertEqual(len(rmdir_calls), 1)
        self.assertEqual(rmdir_calls[0][0], rid)
        self.assertIsInstance(rmdir_calls[0][1], int)
        self.assertNotEqual(rmdir_calls[0][1], unlink_calls[0][1])
        self.assertFalse((self.storage / rid).exists())
        self.assertEqual(pri.list_review_inventory(self.owner_data)['inspected_count'], 0)

    def test_already_gone_is_idempotent(self):
        rid, digest, _folder = self._make_review('b')
        self.assertTrue(prc.cleanup_review_workspace(self.owner_data, rid, digest)['deleted'])
        again = prc.cleanup_review_workspace(self.owner_data, rid, digest)
        self.assertEqual(
            again,
            {
                'schema': prc.CLEANUP_SCHEMA,
                'deleted': False,
                'review_id': rid,
                'reason': prc.REASON_NOT_FOUND,
            },
        )

        empty_owner = self.tmpdir / 'owner-no-storage'
        empty_owner.mkdir(mode=0o700)
        os.chmod(empty_owner, 0o700)
        missing_storage = prc.cleanup_review_workspace(empty_owner, rid, digest)
        self.assertEqual(missing_storage, again)
        self.assertFalse((empty_owner / pri.PROPOSAL_REVIEWS_DIR).exists())

    def test_digest_mismatch_and_tampered_content_refused(self):
        rid, digest, folder = self._make_review('c')
        wrong = ('0' if digest[0] != '0' else '1') * 64
        res = prc.cleanup_review_workspace(self.owner_data, rid, wrong)
        self.assertEqual(
            res,
            {
                'schema': prc.CLEANUP_SCHEMA,
                'deleted': False,
                'review_id': rid,
                'reason': prc.REASON_DIGEST_MISMATCH,
            },
        )
        self._assert_all_files_present(folder)

        rid2, digest2, folder2 = self._make_review('d')
        (folder2 / 'before.txt').write_bytes(b'TAMPERED = 999\n')
        os.chmod(folder2 / 'before.txt', 0o600)
        res2 = prc.cleanup_review_workspace(self.owner_data, rid2, digest2)
        self.assertEqual(res2['deleted'], False)
        self.assertEqual(res2['reason'], prc.REASON_VERIFICATION_FAILED)
        self._assert_all_files_present(folder2)

    def test_extra_file_missing_file_and_bounded_scandir(self):
        rid_extra, digest, folder_extra = self._make_review('e')
        extra = folder_extra / 'extra.txt'
        extra.write_bytes(b'nope\n')
        os.chmod(extra, 0o600)
        res = prc.cleanup_review_workspace(self.owner_data, rid_extra, digest)
        self.assertEqual(res['deleted'], False)
        self.assertEqual(res['reason'], prc.REASON_INCOMPLETE_OR_EXTRA_FILES)
        self.assertTrue(extra.is_file())
        self._assert_all_files_present(folder_extra)

        rid_miss, digest_m, folder_miss = self._make_review('f')
        (folder_miss / 'proposed-after.txt').unlink()
        res_m = prc.cleanup_review_workspace(self.owner_data, rid_miss, digest_m)
        self.assertEqual(res_m['deleted'], False)
        self.assertEqual(res_m['reason'], prc.REASON_INCOMPLETE_OR_EXTRA_FILES)
        self.assertTrue((folder_miss / 'before.txt').is_file())
        self.assertTrue((folder_miss / 'review.json').is_file())

        rid_crowd = self._rid('1')
        crowd = self.storage / rid_crowd
        crowd.mkdir(mode=0o700)
        os.chmod(crowd, 0o700)
        for i in range(60):
            p = crowd / f'f{i:02d}.txt'
            p.write_bytes(b'x')
            os.chmod(p, 0o600)

        scanned = [0]
        real_scandir = os.scandir

        class _CountingScandir:
            def __init__(self, fd):
                self._inner = real_scandir(fd)

            def __enter__(self):
                self._it = self._inner.__enter__()
                return self

            def __exit__(self, *a):
                return self._inner.__exit__(*a)

            def __iter__(self):
                return self

            def __next__(self):
                item = next(self._it)
                scanned[0] += 1
                return item

        with mock.patch('os.scandir', side_effect=_CountingScandir):
            res_c = prc.cleanup_review_workspace(self.owner_data, rid_crowd, digest)
        self.assertEqual(res_c['deleted'], False)
        self.assertEqual(res_c['reason'], prc.REASON_INCOMPLETE_OR_EXTRA_FILES)
        self.assertLessEqual(scanned[0], 10)
        self.assertEqual(len(list(crowd.iterdir())), 60)

    def test_symlink_dir_and_symlink_file_refused(self):
        prop = _make_proposal()
        digest = proposal_digest(prop)
        outside_dir = _write_review(self.tmpdir, 'real-target', prop)
        rid_sym = self._rid('2')
        os.symlink(outside_dir, self.storage / rid_sym)

        res = prc.cleanup_review_workspace(self.owner_data, rid_sym, digest)
        self.assertEqual(res['deleted'], False)
        self.assertEqual(res['reason'], prc.REASON_INVALID_ENTRY_TYPE)
        self.assertTrue((self.storage / rid_sym).is_symlink())
        self._assert_all_files_present(outside_dir)

        rid_inner, digest2, folder = self._make_review('3', prop)
        outside_file = self.project_dir / 'outside.txt'
        outside_file.write_bytes(prop.before.encode('utf-8'))
        os.chmod(outside_file, 0o600)
        (folder / 'before.txt').unlink()
        os.symlink(outside_file, folder / 'before.txt')

        res2 = prc.cleanup_review_workspace(self.owner_data, rid_inner, digest2)
        self.assertEqual(res2['deleted'], False)
        self.assertEqual(res2['reason'], prc.REASON_INVALID_ENTRY_TYPE)
        self.assertTrue((folder / 'before.txt').is_symlink())
        self.assertTrue(outside_file.is_file())

    def test_hardlink_preexisting_and_midway_refused(self):
        rid, digest, folder = self._make_review('4')
        linked = self.project_dir / 'linked-before.txt'
        os.link(folder / 'before.txt', linked)

        res = prc.cleanup_review_workspace(self.owner_data, rid, digest)
        self.assertEqual(res['deleted'], False)
        self.assertEqual(res['reason'], prc.REASON_INVALID_PERMISSIONS)
        self._assert_all_files_present(folder)
        self.assertTrue(linked.is_file())

        rid_mid, digest_mid, folder_mid = self._make_review('5')
        mid_link = self.project_dir / 'mid-link.txt'
        real_unlink = os.unlink
        unlinked = []

        def link_after_first(name, *, dir_fd=None):
            unlinked.append(name)
            real_unlink(name, dir_fd=dir_fd)
            if name == 'before.txt':
                os.link(folder_mid / 'proposed-after.txt', mid_link)

        with mock.patch.object(os, 'unlink', side_effect=link_after_first):
            res_mid = prc.cleanup_review_workspace(self.owner_data, rid_mid, digest_mid)

        self.assertEqual(res_mid['deleted'], False)
        self.assertEqual(res_mid['reason'], prc.REASON_DIRECTORY_CHANGED)
        self.assertEqual(unlinked, ['before.txt'])
        self.assertTrue((folder_mid / 'proposed-after.txt').is_file())
        self.assertTrue((folder_mid / 'review.json').is_file())
        self.assertTrue(mid_link.is_file())

    def test_fifo_dir_and_fifo_file_refused_without_blocking(self):
        prop = _make_proposal()
        digest = proposal_digest(prop)
        rid_fifo = self._rid('6')
        fifo_top = self.storage / rid_fifo
        os.mkfifo(fifo_top, 0o700)
        os.chmod(fifo_top, 0o700)

        res = prc.cleanup_review_workspace(self.owner_data, rid_fifo, digest)
        self.assertEqual(res['deleted'], False)
        self.assertEqual(res['reason'], prc.REASON_INVALID_ENTRY_TYPE)
        self.assertTrue(stat.S_ISFIFO(os.lstat(fifo_top).st_mode))

        rid_inner, digest2, folder = self._make_review('7', prop)
        (folder / 'before.txt').unlink()
        os.mkfifo(folder / 'before.txt', 0o600)
        os.chmod(folder / 'before.txt', 0o600)

        res2 = prc.cleanup_review_workspace(self.owner_data, rid_inner, digest2)
        self.assertEqual(res2['deleted'], False)
        self.assertEqual(res2['reason'], prc.REASON_INVALID_ENTRY_TYPE)
        self.assertTrue(stat.S_ISFIFO(os.lstat(folder / 'before.txt').st_mode))
        self.assertTrue((folder / 'proposed-after.txt').is_file())
        self.assertTrue((folder / 'review.json').is_file())

    def test_wrong_permissions_uid_device_and_storage_refused(self):
        # Wrong dir permissions (0755)
        rid_d, digest_d, folder_d = self._make_review('8')
        os.chmod(folder_d, 0o755)
        res_d = prc.cleanup_review_workspace(self.owner_data, rid_d, digest_d)
        self.assertEqual(res_d['deleted'], False)
        self.assertEqual(res_d['reason'], prc.REASON_INVALID_PERMISSIONS)
        self._assert_all_files_present(folder_d)

        # Wrong file permissions (0644)
        rid_f, digest_f, folder_f = self._make_review('9')
        os.chmod(folder_f / 'before.txt', 0o644)
        res_f = prc.cleanup_review_workspace(self.owner_data, rid_f, digest_f)
        self.assertEqual(res_f['deleted'], False)
        self.assertEqual(res_f['reason'], prc.REASON_INVALID_PERMISSIONS)
        self._assert_all_files_present(folder_f)

        # Simulated wrong uid / wrong device on dir and inner file
        real_lstat = os.lstat
        rid_meta, digest_meta, folder_meta = self._make_review('0')
        for target_name, overrides in (
            (rid_meta, {'st_uid': os.getuid() + 1}),
            (rid_meta, {'st_dev': os.stat(self.storage).st_dev + 99}),
            ('before.txt', {'st_dev': os.stat(self.storage).st_dev + 99}),
        ):
            def fake_lstat(path, *args, _t=target_name, _ov=overrides, **kwargs):
                st = real_lstat(path, *args, **kwargs)
                if path == _t and kwargs.get('dir_fd') is not None:
                    return _StatProxy(st, **_ov)
                return st

            with mock.patch.object(os, 'lstat', side_effect=fake_lstat):
                res_m = prc.cleanup_review_workspace(self.owner_data, rid_meta, digest_meta)
            self.assertEqual(res_m['deleted'], False)
            self.assertEqual(res_m['reason'], prc.REASON_INVALID_PERMISSIONS)
            self._assert_all_files_present(folder_meta)

        # Bad storage permissions -> invalid_storage
        os.chmod(self.storage, 0o755)
        try:
            with self.assertRaises(ProjectReadError):
                prc.cleanup_review_workspace(self.owner_data, rid_meta, digest_meta)
            proc = self._run_cli(
                ['--owner-data', str(self.owner_data)],
                _make_frame(rid_meta, digest_meta),
            )
            self.assertEqual(proc.returncode, 1)
            self.assertEqual(
                json.loads(proc.stdout.decode('ascii')),
                {
                    'schema': prc.CLEANUP_SCHEMA,
                    'deleted': False,
                    'review_id': rid_meta,
                    'reason': prc.REASON_INVALID_STORAGE,
                },
            )
        finally:
            os.chmod(self.storage, 0o700)
        self._assert_all_files_present(folder_meta)

    def test_race_swap_dir_between_verification_and_unlink(self):
        prop = _make_proposal()
        rid, digest, orig_dir = self._make_review('a', prop)
        # Decoy is itself a valid review dir so only inode-identity checking catches the swap.
        decoy_dir = _write_review(self.tmpdir, 'decoy-swap', prop)
        moved_orig = self.tmpdir / 'moved-orig'
        real_inspect = pri.inspect_pinned_review_workspace
        unlink_calls, rmdir_calls = [], []

        def swap_after_inspect(*args, **kwargs):
            record = real_inspect(*args, **kwargs)
            os.rename(orig_dir, moved_orig)
            os.rename(decoy_dir, self.storage / rid)
            return record

        with (
            mock.patch.object(prc, 'inspect_pinned_review_workspace', side_effect=swap_after_inspect),
            mock.patch.object(os, 'unlink', side_effect=lambda *a, **k: unlink_calls.append(a)),
            mock.patch.object(os, 'rmdir', side_effect=lambda *a, **k: rmdir_calls.append(a)),
        ):
            res = prc.cleanup_review_workspace(self.owner_data, rid, digest)

        self.assertEqual(res['deleted'], False)
        self.assertEqual(res['reason'], prc.REASON_DIRECTORY_CHANGED)
        self.assertEqual(unlink_calls, [])
        self.assertEqual(rmdir_calls, [])
        self._assert_all_files_present(moved_orig)
        self._assert_all_files_present(self.storage / rid)

    def test_race_swap_dir_between_unlink_and_rmdir_and_late_extra_file(self):
        # Case 1: swap directory after unlinking 3rd file, before rmdir
        rid1, digest1, orig1 = self._make_review('b')
        decoy_empty = self.tmpdir / 'decoy-empty'
        decoy_empty.mkdir(mode=0o700)
        os.chmod(decoy_empty, 0o700)
        decoy_ino = os.stat(decoy_empty).st_ino
        emptied_orig = self.tmpdir / 'emptied-orig'
        real_unlink = os.unlink
        rmdir_calls = []

        def swap_after_last_unlink(name, *, dir_fd=None):
            real_unlink(name, dir_fd=dir_fd)
            if name == pri.FIXED_REVIEW_FILES[-1]:
                os.rename(orig1, emptied_orig)
                os.rename(decoy_empty, self.storage / rid1)

        with (
            mock.patch.object(os, 'unlink', side_effect=swap_after_last_unlink),
            mock.patch.object(os, 'rmdir', side_effect=lambda *a, **k: rmdir_calls.append(a)),
        ):
            res1 = prc.cleanup_review_workspace(self.owner_data, rid1, digest1)

        self.assertEqual(res1['deleted'], False)
        self.assertEqual(res1['reason'], prc.REASON_DIRECTORY_CHANGED)
        self.assertEqual(rmdir_calls, [])
        self.assertTrue((self.storage / rid1).is_dir())
        self.assertEqual(os.stat(self.storage / rid1).st_ino, decoy_ino)

        # Case 2: new file appears after 3rd unlink, before rmdir
        rid2, digest2, orig2 = self._make_review('c')
        rmdir_calls2 = []

        def drop_late_file(name, *, dir_fd=None):
            real_unlink(name, dir_fd=dir_fd)
            if name == pri.FIXED_REVIEW_FILES[-1]:
                late = orig2 / 'late.txt'
                late.write_bytes(b'keep')
                os.chmod(late, 0o600)

        with (
            mock.patch.object(os, 'unlink', side_effect=drop_late_file),
            mock.patch.object(os, 'rmdir', side_effect=lambda *a, **k: rmdir_calls2.append(a)),
        ):
            res2 = prc.cleanup_review_workspace(self.owner_data, rid2, digest2)

        self.assertEqual(res2['deleted'], False)
        self.assertEqual(res2['reason'], prc.REASON_DIRECTORY_CHANGED)
        self.assertEqual(rmdir_calls2, [])
        self.assertTrue((orig2 / 'late.txt').is_file())

    def test_cancel_midway_and_deadline_expiry(self):
        # Cancel during verification hashing
        rid1, digest1, folder1 = self._make_review('d', _make_multichunk_proposal())
        cancel1 = threading.Event()
        real_sha = pri._sha256_chunked

        def cancel_in_hash(*args, **kwargs):
            cancel1.set()
            return real_sha(*args, **kwargs)

        with mock.patch.object(pri, '_sha256_chunked', side_effect=cancel_in_hash):
            res1 = prc.cleanup_review_workspace(
                self.owner_data, rid1, digest1, cancel=cancel1
            )
        self.assertEqual(res1['deleted'], False)
        self.assertEqual(res1['reason'], prc.REASON_CANCELED)
        self._assert_all_files_present(folder1)

        # Cancel mid-way between unlinks (after first unlink)
        rid2, digest2, folder2 = self._make_review('e')
        cancel2 = threading.Event()
        real_unlink = os.unlink
        unlinked = []

        def cancel_after_first(name, *, dir_fd=None):
            unlinked.append(name)
            real_unlink(name, dir_fd=dir_fd)
            if name == 'before.txt':
                cancel2.set()

        with mock.patch.object(os, 'unlink', side_effect=cancel_after_first):
            res2 = prc.cleanup_review_workspace(
                self.owner_data, rid2, digest2, cancel=cancel2
            )
        self.assertEqual(res2['deleted'], False)
        self.assertEqual(res2['reason'], prc.REASON_CANCELED)
        self.assertEqual(unlinked, ['before.txt'])
        self.assertTrue((folder2 / 'proposed-after.txt').is_file())
        self.assertTrue((folder2 / 'review.json').is_file())

        # Cancel right before rmdir
        rid3, digest3, folder3 = self._make_review('f')
        cancel3 = threading.Event()
        rmdir_calls = []

        def cancel_before_rmdir(name, *, dir_fd=None):
            real_unlink(name, dir_fd=dir_fd)
            if name == pri.FIXED_REVIEW_FILES[-1]:
                cancel3.set()

        with (
            mock.patch.object(os, 'unlink', side_effect=cancel_before_rmdir),
            mock.patch.object(os, 'rmdir', side_effect=lambda *a, **k: rmdir_calls.append(a)),
        ):
            res3 = prc.cleanup_review_workspace(
                self.owner_data, rid3, digest3, cancel=cancel3
            )
        self.assertEqual(res3['deleted'], False)
        self.assertEqual(res3['reason'], prc.REASON_CANCELED)
        self.assertEqual(rmdir_calls, [])
        self.assertTrue(folder3.is_dir())

        # Expired deadline
        rid4, digest4, folder4 = self._make_review('1')
        sfd = os.open(
            self.storage,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
        )
        try:
            res4 = prc.cleanup_pinned_review_workspace(
                sfd, rid4, digest4, deadline=time.monotonic() - 1.0
            )
        finally:
            os.close(sfd)
        self.assertEqual(res4['deleted'], False)
        self.assertEqual(res4['reason'], prc.REASON_TIMED_OUT)
        self._assert_all_files_present(folder4)

    def test_no_deletion_outside_target_and_no_path_or_content_echo(self):
        secret_rel = 'src/secret_marker_file.py'
        secret_before = 'SECRET_BEFORE_PAYLOAD = 1\n'
        secret_after = 'SECRET_AFTER_PAYLOAD = 2\n'
        prop = _make_proposal(secret_rel, secret_before, secret_after)

        proj_file = self.project_dir / secret_rel
        proj_file.parent.mkdir(parents=True, mode=0o700)
        proj_file.write_text(secret_before, encoding='utf-8')
        proj_ino = os.stat(proj_file).st_ino

        rid_target, digest_target, _ = self._make_review('2', prop)
        rid_sibling, _digest_sib, sibling_dir = self._make_review('3', prop)
        sibling_inodes = {n: os.stat(sibling_dir / n).st_ino for n in pri.FIXED_REVIEW_FILES}

        # Check both failure (mismatch) and success CLI outputs never echo paths or contents.
        wrong_digest = ('0' if digest_target[0] != '0' else '1') * 64
        for expected_rc, digest_arg in ((1, wrong_digest), (0, digest_target)):
            proc = self._run_cli(
                ['--owner-data', str(self.owner_data)],
                _make_frame(rid_target, digest_arg),
            )
            self.assertEqual(proc.returncode, expected_rc)
            self.assertEqual(proc.stderr, b'')
            self.assertLessEqual(len(proc.stdout), prc.MAX_OUTPUT_BYTES)
            combined = proc.stdout.decode('ascii')
            for forbidden in (
                str(self.tmpdir),
                str(self.owner_data),
                pri.PROPOSAL_REVIEWS_DIR,
                'secret_marker_file',
                'SECRET_BEFORE_PAYLOAD',
                'SECRET_AFTER_PAYLOAD',
            ):
                self.assertNotIn(forbidden, combined)

        self.assertFalse((self.storage / rid_target).exists())
        self.assertTrue(sibling_dir.is_dir())
        self.assertEqual(
            {n: os.stat(sibling_dir / n).st_ino for n in pri.FIXED_REVIEW_FILES},
            sibling_inodes,
        )
        self.assertEqual(os.stat(proj_file).st_ino, proj_ino)
        self.assertEqual(proj_file.read_text(encoding='utf-8'), secret_before)

    def test_cli_and_frame_strictness(self):
        rid, digest, folder = self._make_review('4')
        base_frame = _make_frame(rid, digest)

        # Valid compact frame & exact 256-byte boundary accepted; 257-byte rejected.
        decoded = prc.decode_cleanup_frame(base_frame)
        self.assertEqual(decoded['review_id'], rid)
        self.assertEqual(decoded['proposal_digest'], digest)

        pad_len = prc.MAX_REQUEST_FRAME_BYTES - len(base_frame)
        exact_256 = base_frame[:-2] + (b' ' * pad_len) + b'}\n'
        self.assertEqual(len(exact_256), prc.MAX_REQUEST_FRAME_BYTES)
        self.assertEqual(prc.decode_cleanup_frame(exact_256)['review_id'], rid)

        over_256 = base_frame[:-2] + (b' ' * (pad_len + 1)) + b'}\n'
        self.assertEqual(len(over_256), prc.MAX_REQUEST_FRAME_BYTES + 1)

        bad_frames = [
            b'',
            b'\n',
            base_frame[:-1],  # missing trailing newline
            base_frame[:-1] + b'\r\n',
            base_frame + base_frame,  # two lines
            base_frame[:-2] + b'\x00}\n',
            over_256,
            (
                f'{{"schema":"{prc.REQUEST_SCHEMA}","review_id":"{rid}",'
                f'"proposal_digest":"{digest}","extra":1}}\n'
            ).encode('ascii'),
            (
                f'{{"schema":"{prc.REQUEST_SCHEMA}","review_id":"{rid}"}}\n'
            ).encode('ascii'),
            (
                f'{{"schema":"{prc.REQUEST_SCHEMA}","schema":"{prc.REQUEST_SCHEMA}",'
                f'"review_id":"{rid}","proposal_digest":"{digest}"}}\n'
            ).encode('ascii'),
            (
                f'{{"schema":"navi-review-inventory-request-v1",'
                f'"review_id":"{rid}","proposal_digest":"{digest}"}}\n'
            ).encode('ascii'),
            _make_frame('../' + rid, digest),
            _make_frame('review-' + 'A' * 32, digest),
            _make_frame('review-' + 'a' * 31, digest),
            _make_frame(rid, 'A' * 64),
            _make_frame(rid, 'a' * 63),
            (
                f'{{"schema":"{prc.REQUEST_SCHEMA}","review_id":"{rid}",'
                f'"proposal_digest":NaN}}\n'
            ).encode('ascii'),
            (
                f'{{"schema":"{prc.REQUEST_SCHEMA}","review_id":"{rid}",'
                f'"proposal_digest":"{digest}"}}\n'
            ).encode('ascii').replace(b'review-', 'réview-'.encode('utf-8')),
        ]
        for bad in bad_frames:
            with self.assertRaises(ProjectReadError):
                prc.decode_cleanup_frame(bad)

        # Strict CLI argv checks
        bad_argvs = [
            [],
            ['--owner-data'],
            ['--owner-data', ''],
            ['--owner-data', str(self.owner_data), '--extra'],
            ['--root', str(self.owner_data)],
        ]
        for argv in bad_argvs:
            proc = self._run_cli(argv, base_frame)
            self.assertEqual(proc.returncode, 1)
            self.assertEqual(
                json.loads(proc.stdout.decode('ascii')),
                {
                    'schema': prc.CLEANUP_SCHEMA,
                    'deleted': False,
                    'review_id': None,
                    'reason': prc.REASON_INVALID_REQUEST,
                },
            )

        # Bad stdin frames via CLI must fail closed and delete nothing
        for bad in (b'', base_frame[:-1], over_256, bad_frames[7], bad_frames[9], bad_frames[11]):
            proc = self._run_cli(['--owner-data', str(self.owner_data)], bad)
            self.assertEqual(proc.returncode, 1)
            payload = json.loads(proc.stdout.decode('ascii'))
            self.assertEqual(
                payload,
                {
                    'schema': prc.CLEANUP_SCHEMA,
                    'deleted': False,
                    'review_id': None,
                    'reason': prc.REASON_INVALID_REQUEST,
                },
            )
        self._assert_all_files_present(folder)

        # Valid CLI cleanup returns 0; second call (not_found) is idempotent and returns 0
        ok1 = self._run_cli(['--owner-data', str(self.owner_data)], base_frame)
        self.assertEqual(ok1.returncode, 0)
        self.assertEqual(
            json.loads(ok1.stdout.decode('ascii')),
            {'schema': prc.CLEANUP_SCHEMA, 'deleted': True, 'review_id': rid},
        )
        ok2 = self._run_cli(['--owner-data', str(self.owner_data)], base_frame)
        self.assertEqual(ok2.returncode, 0)
        self.assertEqual(
            json.loads(ok2.stdout.decode('ascii')),
            {
                'schema': prc.CLEANUP_SCHEMA,
                'deleted': False,
                'review_id': rid,
                'reason': prc.REASON_NOT_FOUND,
            },
        )

        # Serializer schema strictness
        for invalid_out in (
            {'schema': prc.CLEANUP_SCHEMA, 'deleted': True, 'review_id': rid, 'reason': 'not_found'},
            {'schema': prc.CLEANUP_SCHEMA, 'deleted': False, 'review_id': rid, 'reason': 'freeform'},
            {'schema': prc.CLEANUP_SCHEMA, 'deleted': False, 'review_id': None, 'reason': 'not_found'},
            {'schema': prc.CLEANUP_SCHEMA, 'deleted': True, 'review_id': rid, 'path': '/tmp'},
        ):
            with self.assertRaises(ProjectReadError):
                prc.serialize_cleanup_result(invalid_out)


if __name__ == '__main__':
    unittest.main()
