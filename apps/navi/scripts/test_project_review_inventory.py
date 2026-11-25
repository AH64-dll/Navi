"""Synthetic local verification tests for project_review_inventory.py.

Read-only owner metadata inventory only: no source contents returned, no writes,
deletes, cleanup, or project execution. Local tests are verification, not proof
of safety.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

WORKERS_DIR = Path(__file__).resolve().parent.parent / 'workers'
sys.path.insert(0, str(WORKERS_DIR))

from project_edit_reviews import proposal_digest
from project_edits import ProjectReadError, propose_edit
import project_review_inventory as pri
from project_workspace import create_review_workspace, owner_review_storage


def _sha256(text):
    return hashlib.sha256(text.encode('utf-8')).hexdigest()


def _make_proposal(path, before, after):
    return propose_edit(path, _sha256(before), before, after)


def _make_multichunk_proposal(path='src/large.py'):
    """Build a valid proposal spanning multiple read and hash chunks."""
    lines = [f'LINE_{i:02d} = "{"x" * 64}"\n' for i in range(40)]
    before = ''.join(lines)
    after_lines = list(lines)
    after_lines[0] = f'LINE_00 = "{"y" * 64}"\n'
    after = ''.join(after_lines)
    return _make_proposal(path, before, after)


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


def _write_synthetic_valid_review(storage, review_id, proposal):
    """Write an exact valid navi-private-review-v1 workspace quickly for cap tests."""
    folder = storage / review_id
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
    entries = [
        ('before.txt', proposal.before.encode('utf-8')),
        ('proposed-after.txt', proposal.after.encode('utf-8')),
        ('review.json', (json.dumps(manifest, ensure_ascii=True, indent=2) + '\n').encode('utf-8')),
    ]
    for name, raw in entries:
        target = folder / name
        target.write_bytes(raw)
        os.chmod(target, 0o600)
    return folder


def _make_crowded_dir(path, count=50):
    path.mkdir(mode=0o700)
    os.chmod(path, 0o700)
    for i in range(count):
        item = path / f'extra_{i:03d}.txt'
        item.write_bytes(b'x')
        os.chmod(item, 0o600)
    return path


def _make_counting_scandir(real_scandir, counter):
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
            counter[0] += 1
            return item

    return CountingScandir


class ReviewInventoryTests(unittest.TestCase):
    def setUp(self):
        parent = _find_safe_test_parent()
        self.base = Path(tempfile.mkdtemp(prefix='.navi-inv-test-', dir=str(parent)))
        os.chmod(self.base, 0o700)
        self.data_root = self.base / 'owner-data'
        self.data_root.mkdir(mode=0o700)
        os.chmod(self.data_root, 0o700)

    def tearDown(self):
        # Restore permissions in case a test modified them before cleanup.
        for root, dirs, files in os.walk(self.base, topdown=True, followlinks=False):
            try:
                os.chmod(root, 0o700)
            except OSError:
                pass
            for d in dirs:
                p = Path(root) / d
                if not p.is_symlink():
                    try:
                        os.chmod(p, 0o700)
                    except OSError:
                        pass
            for f in files:
                p = Path(root) / f
                if not p.is_symlink():
                    try:
                        os.chmod(p, 0o600)
                    except OSError:
                        pass
        shutil.rmtree(self.base, ignore_errors=True)

    def _reviews_dir(self):
        return owner_review_storage(self.data_root)

    def _reset_reviews_dir(self):
        target = self.data_root / 'proposal-reviews'
        if target.is_symlink() or target.is_file():
            target.unlink()
        elif target.is_dir():
            shutil.rmtree(target)
        return self._reviews_dir()

    def test_missing_proposal_reviews_returns_empty_without_creating_directory(self):
        result = pri.list_review_inventory(self.data_root)
        self.assertEqual(result['schema'], pri.INVENTORY_SCHEMA)
        self.assertFalse(result['applied'])
        self.assertFalse(result['source_contents_exposed'])
        self.assertFalse(result['storage_present'])
        self.assertEqual(result['inspected_count'], 0)
        self.assertEqual(result['valid_count'], 0)
        self.assertEqual(result['invalid_count'], 0)
        self.assertEqual(result['items'], [])
        # Strictly read-only: proposal-reviews must NOT have been created.
        self.assertFalse((self.data_root / 'proposal-reviews').exists())

    def test_valid_reviews_return_metadata_and_bytes_only_without_source_or_invented_time(self):
        storage = self._reviews_dir()
        before_a = 'SECRET_BEFORE_ALPHA = "do-not-expose-1"\n'
        after_a = 'SECRET_AFTER_ALPHA = "do-not-expose-2"\n'
        p_a = _make_proposal('src/alpha.py', before_a, after_a)
        p_b = _make_proposal('cfg/beta.json', '{"k": 1}\n', '{"k": 2}\n')

        r_a = create_review_workspace(storage, p_a, request_id='a' * 32)
        r_b = create_review_workspace(storage, p_b, request_id='b' * 32)

        # Explicit filesystem mtimes must never be fabricated into a creation time.
        os.utime(storage / r_a['review_id'], (1700000000, 1700000000))
        os.utime(storage / r_a['review_id'] / 'review.json', (1700000000, 1700000000))

        with mock.patch.object(
            os, 'listdir', side_effect=AssertionError('must never call unbounded os.listdir')
        ):
            inv = pri.list_review_inventory(self.data_root)

        self.assertTrue(inv['storage_present'])
        self.assertFalse(inv['applied'])
        self.assertFalse(inv['source_contents_exposed'])
        self.assertEqual(inv['inspected_count'], 2)
        self.assertEqual(inv['valid_count'], 2)
        self.assertEqual(inv['invalid_count'], 0)

        by_id = {item['review_id']: item for item in inv['items']}
        self.assertEqual(set(by_id), {r_a['review_id'], r_b['review_id']})
        item_a = by_id[r_a['review_id']]
        expected_keys = {
            'review_id',
            'valid',
            'status',
            'applied',
            'requires_fresh_validation',
            'original_path',
            'proposal_digest',
            'before_sha256',
            'after_sha256',
            'before_bytes',
            'after_bytes',
            'files',
        }
        self.assertEqual(set(item_a), expected_keys)
        self.assertTrue(item_a['valid'])
        self.assertEqual(item_a['status'], 'valid')
        self.assertFalse(item_a['applied'])
        self.assertTrue(item_a['requires_fresh_validation'])
        self.assertEqual(item_a['original_path'], 'src/alpha.py')
        self.assertEqual(item_a['proposal_digest'], proposal_digest(p_a))
        self.assertEqual(item_a['before_sha256'], p_a.before_sha256)
        self.assertEqual(item_a['after_sha256'], p_a.after_sha256)
        self.assertEqual(item_a['before_bytes'], len(before_a.encode('utf-8')))
        self.assertEqual(item_a['after_bytes'], len(after_a.encode('utf-8')))
        self.assertEqual(item_a['files'], ['before.txt', 'proposed-after.txt', 'review.json'])

        for forbidden_key in (
            'before',
            'after',
            'proposal',
            'created',
            'created_at',
            'mtime',
            'ctime',
            'timestamp',
            'request_id',
        ):
            self.assertNotIn(forbidden_key, item_a)

        serialized = pri.serialize_inventory_result(inv)
        self.assertLessEqual(len(serialized.encode('ascii')), pri.MAX_OUTPUT_BYTES)
        self.assertNotIn('SECRET_BEFORE_ALPHA', serialized)
        self.assertNotIn('SECRET_AFTER_ALPHA', serialized)
        self.assertNotIn('do-not-expose', serialized)
        self.assertNotIn(str(self.data_root), serialized)

    def test_direct_pinned_inspector_verifies_via_fd_and_rejects_bad_fd_or_id(self):
        storage = self._reviews_dir()
        p = _make_proposal('pinned.py', 'a = 1\n', 'a = 2\n')
        created = create_review_workspace(storage, p, request_id='e' * 32)

        storage_fd = os.open(
            storage,
            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
        )
        try:
            with mock.patch.object(
                os, 'listdir', side_effect=AssertionError('must never call unbounded os.listdir')
            ):
                rec = pri.inspect_pinned_review_workspace(storage_fd, created['review_id'])
            self.assertTrue(rec['valid'])
            self.assertEqual(rec['original_path'], 'pinned.py')
            self.assertEqual(rec['proposal_digest'], proposal_digest(p))
            self.assertNotIn('created', rec)

            with self.assertRaises(ProjectReadError):
                pri.inspect_pinned_review_workspace(storage_fd, '../escape')
            with self.assertRaises(ProjectReadError):
                pri.inspect_pinned_review_workspace(-1, created['review_id'])

            os.chmod(storage, 0o755)
            try:
                with self.assertRaises(ProjectReadError):
                    pri.inspect_pinned_review_workspace(storage_fd, created['review_id'])
            finally:
                os.chmod(storage, 0o700)
        finally:
            os.close(storage_fd)

    def test_invalid_and_partial_reviews_report_bounded_nonsensitive_status_without_leaking_text(self):
        storage = self._reviews_dir()
        valid_p = _make_proposal('ok.py', 'x = 1\n', 'x = 2\n')
        r_valid = create_review_workspace(storage, valid_p, request_id='1' * 32)

        # Partial review (missing review.json)
        r_partial = create_review_workspace(storage, valid_p, request_id='2' * 32)
        (storage / r_partial['review_id'] / 'review.json').unlink()

        # Extra file inside review directory
        r_extra = create_review_workspace(storage, valid_p, request_id='3' * 32)
        extra_file = storage / r_extra['review_id'] / 'stray.txt'
        extra_file.write_text('LEAK_EXTRA_SECRET', encoding='utf-8')
        os.chmod(extra_file, 0o600)

        # Tampered snapshot content containing sensitive text
        r_tampered = create_review_workspace(storage, valid_p, request_id='4' * 32)
        (storage / r_tampered['review_id'] / 'proposed-after.txt').write_text(
            'LEAK_TAMPERED_SECRET_VALUE\n', encoding='utf-8'
        )

        # Bad directory permissions (0755)
        r_perms = create_review_workspace(storage, valid_p, request_id='5' * 32)
        os.chmod(storage / r_perms['review_id'], 0o755)

        # Unreadable directory permissions (0000)
        r_zero = create_review_workspace(storage, valid_p, request_id='6' * 32)
        os.chmod(storage / r_zero['review_id'], 0o000)

        inv = pri.list_review_inventory(self.data_root)
        self.assertEqual(inv['inspected_count'], 6)
        self.assertEqual(inv['valid_count'], 1)
        self.assertEqual(inv['invalid_count'], 5)

        by_id = {item['review_id']: item for item in inv['items']}
        self.assertTrue(by_id[r_valid['review_id']]['valid'])
        self.assertEqual(by_id[r_partial['review_id']]['reason'], 'incomplete_or_extra_files')
        self.assertEqual(by_id[r_extra['review_id']]['reason'], 'incomplete_or_extra_files')
        self.assertEqual(by_id[r_tampered['review_id']]['reason'], 'verification_failed')
        self.assertEqual(by_id[r_perms['review_id']]['reason'], 'invalid_permissions')
        self.assertEqual(by_id[r_zero['review_id']]['reason'], 'invalid_permissions')

        for bad_id in (
            r_partial['review_id'],
            r_extra['review_id'],
            r_tampered['review_id'],
            r_perms['review_id'],
            r_zero['review_id'],
        ):
            rec = by_id[bad_id]
            self.assertFalse(rec['valid'])
            self.assertEqual(rec['status'], 'invalid')
            self.assertEqual(set(rec), {'review_id', 'valid', 'status', 'reason', 'applied'})

        serialized = json.dumps(inv)
        self.assertNotIn('LEAK_EXTRA_SECRET', serialized)
        self.assertNotIn('LEAK_TAMPERED_SECRET_VALUE', serialized)

    def test_manifest_strict_schema_rejects_extra_or_tampered_fields_and_never_invents_created_time(self):
        storage = self._reviews_dir()
        p = _make_proposal('meta.py', 'a = 1\n', 'a = 2\n')

        def make_modified_manifest(rid, mutator):
            folder = _write_synthetic_valid_review(storage, rid, p)
            manifest_path = folder / 'review.json'
            data = json.loads(manifest_path.read_text(encoding='utf-8'))
            mutator(data)
            manifest_path.write_bytes(
                (json.dumps(data, ensure_ascii=True, indent=2) + '\n').encode('utf-8')
            )
            os.chmod(manifest_path, 0o600)

        # Extra 'created' key rejected by strict navi-private-review-v1 schema
        id_created = f'review-{1:032x}'
        make_modified_manifest(id_created, lambda m: m.__setitem__('created', '2026-11-24T12:00:00Z'))

        # Extra 'request_id' key rejected
        id_req = f'review-{2:032x}'
        make_modified_manifest(id_req, lambda m: m.__setitem__('request_id', 'c' * 32))

        # applied: True rejected
        id_applied = f'review-{3:032x}'
        make_modified_manifest(id_applied, lambda m: m.__setitem__('applied', True))

        # requires_fresh_validation: False rejected
        id_fresh = f'review-{4:032x}'
        make_modified_manifest(id_fresh, lambda m: m.__setitem__('requires_fresh_validation', False))

        # Wrong schema rejected
        id_schema = f'review-{5:032x}'
        make_modified_manifest(id_schema, lambda m: m.__setitem__('schema', 'navi-private-review-v2'))

        # Mismatched proposal_digest rejected
        id_digest = f'review-{6:032x}'
        make_modified_manifest(id_digest, lambda m: m.__setitem__('proposal_digest', '0' * 64))

        # Duplicate JSON key rejected
        id_dup = f'review-{7:032x}'
        dup_folder = _write_synthetic_valid_review(storage, id_dup, p)
        raw_manifest = (dup_folder / 'review.json').read_text(encoding='utf-8')
        dup_raw = raw_manifest.replace(
            '"applied": false,', '"applied": false, "applied": false,', 1
        )
        (dup_folder / 'review.json').write_bytes(dup_raw.encode('utf-8'))
        os.chmod(dup_folder / 'review.json', 0o600)

        # Invalid UTF-8 in before.txt rejected
        id_utf8 = f'review-{8:032x}'
        utf8_folder = _write_synthetic_valid_review(storage, id_utf8, p)
        (utf8_folder / 'before.txt').write_bytes(b'\xff\xfeinvalid')
        os.chmod(utf8_folder / 'before.txt', 0o600)

        # Oversized (> 16384 bytes) before.txt rejected
        id_big = f'review-{9:032x}'
        big_folder = _write_synthetic_valid_review(storage, id_big, p)
        (big_folder / 'before.txt').write_bytes(b'a' * (pri.MAX_REVIEW_FILE_BYTES + 1))
        os.chmod(big_folder / 'before.txt', 0o600)

        inv = pri.list_review_inventory(self.data_root)
        self.assertEqual(inv['valid_count'], 0)
        self.assertEqual(inv['invalid_count'], 9)
        by_id = {item['review_id']: item for item in inv['items']}
        for rid in (
            id_created,
            id_req,
            id_applied,
            id_fresh,
            id_schema,
            id_digest,
            id_dup,
            id_utf8,
            id_big,
        ):
            self.assertFalse(by_id[rid]['valid'])
            self.assertEqual(by_id[rid]['reason'], 'verification_failed')

    def test_symlink_hardlink_fifo_and_bad_permissions_inside_review_dir_reported_invalid_without_following(self):
        storage = self._reviews_dir()
        p = _make_proposal('app.py', 'a = 1\n', 'a = 2\n')
        outside = self.base / 'outside-secret.txt'
        outside.write_text('OUTSIDE_SECRET_NEVER_READ\n', encoding='utf-8')
        os.chmod(outside, 0o600)

        # Symlink inside review dir
        r_sym = create_review_workspace(storage, p, request_id='1' * 32)
        sym_target = storage / r_sym['review_id'] / 'before.txt'
        sym_target.unlink()
        sym_target.symlink_to(outside)

        # Hardlink inside review dir
        r_hard = create_review_workspace(storage, p, request_id='2' * 32)
        hard_target = storage / r_hard['review_id'] / 'before.txt'
        hard_target.unlink()
        os.link(outside, hard_target)

        # FIFO inside review dir (must never block by opening)
        r_fifo = create_review_workspace(storage, p, request_id='3' * 32)
        fifo_target = storage / r_fifo['review_id'] / 'before.txt'
        fifo_target.unlink()
        os.mkfifo(fifo_target, 0o600)

        # Non-0600 file inside review dir
        r_mode = create_review_workspace(storage, p, request_id='4' * 32)
        os.chmod(storage / r_mode['review_id'] / 'before.txt', 0o644)

        # Subdirectory named before.txt inside review dir
        r_subdir = create_review_workspace(storage, p, request_id='5' * 32)
        subdir_target = storage / r_subdir['review_id'] / 'before.txt'
        subdir_target.unlink()
        subdir_target.mkdir(mode=0o700)

        inv = pri.list_review_inventory(self.data_root)
        self.assertEqual(inv['valid_count'], 0)
        self.assertEqual(inv['invalid_count'], 5)
        by_id = {item['review_id']: item for item in inv['items']}
        self.assertEqual(by_id[r_sym['review_id']]['reason'], 'invalid_entry_type')
        self.assertEqual(by_id[r_fifo['review_id']]['reason'], 'invalid_entry_type')
        self.assertEqual(by_id[r_subdir['review_id']]['reason'], 'invalid_entry_type')
        self.assertEqual(by_id[r_hard['review_id']]['reason'], 'invalid_permissions')
        self.assertEqual(by_id[r_mode['review_id']]['reason'], 'invalid_permissions')
        self.assertNotIn('OUTSIDE_SECRET_NEVER_READ', json.dumps(inv))

    def test_symlink_or_non_directory_at_review_id_reported_invalid_without_following(self):
        storage = self._reviews_dir()
        outside_dir = self.base / 'outside-dir'
        outside_dir.mkdir(mode=0o700)
        os.chmod(outside_dir, 0o700)

        sym_id = 'review-' + ('a' * 32)
        (storage / sym_id).symlink_to(outside_dir, target_is_directory=True)

        file_id = 'review-' + ('b' * 32)
        (storage / file_id).write_text('not a directory', encoding='utf-8')
        os.chmod(storage / file_id, 0o600)

        fifo_id = 'review-' + ('c' * 32)
        os.mkfifo(storage / fifo_id, 0o600)

        inv = pri.list_review_inventory(self.data_root)
        self.assertEqual(inv['inspected_count'], 3)
        self.assertEqual(inv['valid_count'], 0)
        self.assertEqual(inv['invalid_count'], 3)
        for rid in (sym_id, file_id, fifo_id):
            rec = next(item for item in inv['items'] if item['review_id'] == rid)
            self.assertFalse(rec['valid'])
            self.assertEqual(rec['reason'], 'invalid_entry_type')

    def test_unknown_non_review_entry_fails_closed_without_following_or_echoing_name(self):
        storage = self._reviews_dir()
        for entry_kind in ('file', 'dir', 'symlink', 'fifo'):
            for existing in list(storage.iterdir()):
                if existing.is_dir() and not existing.is_symlink():
                    shutil.rmtree(existing)
                else:
                    existing.unlink()
            target = storage / f'unexpected-secret-{entry_kind}'
            if entry_kind == 'file':
                target.write_text('x', encoding='utf-8')
            elif entry_kind == 'dir':
                target.mkdir(mode=0o700)
            elif entry_kind == 'symlink':
                target.symlink_to(self.base, target_is_directory=True)
            else:
                os.mkfifo(target, 0o600)

            with self.assertRaises(ProjectReadError) as ctx:
                pri.list_review_inventory(self.data_root)
            self.assertNotIn('unexpected-secret', str(ctx.exception))

    def test_unsafe_or_symlinked_or_nonprivate_storage_root_fails_closed(self):
        # Symlink at proposal-reviews refused
        real_target = self.base / 'elsewhere'
        real_target.mkdir(mode=0o700)
        os.chmod(real_target, 0o700)
        (self.data_root / 'proposal-reviews').symlink_to(real_target, target_is_directory=True)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.data_root)
        (self.data_root / 'proposal-reviews').unlink()

        # Non-0700 proposal-reviews refused
        reviews = self.data_root / 'proposal-reviews'
        reviews.mkdir(mode=0o755)
        os.chmod(reviews, 0o755)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.data_root)
        os.chmod(reviews, 0o700)

        # Non-private owner data root refused
        os.chmod(self.data_root, 0o755)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.data_root)
        os.chmod(self.data_root, 0o700)

        # Symlinked or missing data_root refused
        sym_root = self.base / 'sym-owner-data'
        sym_root.symlink_to(self.data_root, target_is_directory=True)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(sym_root)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.base / 'does-not-exist')

        # Relative or world-writable ancestor (/tmp) refused
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory('relative/path')
        with tempfile.TemporaryDirectory() as world_tmp:
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(world_tmp)

    def test_caps_allow_32_valid_plus_32_invalid_and_refuse_33_valid_or_65_children(self):
        storage = self._reviews_dir()
        p = _make_proposal('mod.py', 'v = 1\n', 'v = 2\n')

        # 32 valid + 32 invalid = 64 inspected children succeeds
        for i in range(32):
            _write_synthetic_valid_review(storage, f'review-{i:032x}', p)
        for i in range(32, 64):
            empty_dir = storage / f'review-{i:032x}'
            empty_dir.mkdir(mode=0o700)
            os.chmod(empty_dir, 0o700)

        inv = pri.list_review_inventory(self.data_root)
        self.assertEqual(inv['inspected_count'], 64)
        self.assertEqual(inv['valid_count'], 32)
        self.assertEqual(inv['invalid_count'], 32)
        self.assertLessEqual(
            len(pri.serialize_inventory_result(inv).encode('ascii')), pri.MAX_OUTPUT_BYTES
        )

        # 65th child refuses overflow rather than silently returning partial results
        overflow_dir = storage / f'review-{64:032x}'
        overflow_dir.mkdir(mode=0o700)
        os.chmod(overflow_dir, 0o700)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.data_root)
        overflow_dir.rmdir()

        # Converting one invalid into a 33rd valid (64 total children, 33 valid) refuses overflow
        shutil.rmtree(storage / f'review-{32:032x}')
        _write_synthetic_valid_review(storage, f'review-{32:032x}', p)
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.data_root)

    def test_bounded_scandir_stops_at_65_entries_and_4_inner_files(self):
        storage = self._reviews_dir()
        # Outer overflow: 80 entries, counting scandir must stop at 65.
        for i in range(80):
            d = storage / f'review-{i:032x}'
            d.mkdir(mode=0o700)
            os.chmod(d, 0o700)

        seen_outer = [0]
        real_scandir = pri.os.scandir
        counting_outer = _make_counting_scandir(real_scandir, seen_outer)

        with mock.patch.object(pri.os, 'scandir', counting_outer), mock.patch.object(
            os, 'listdir', side_effect=AssertionError('must never call unbounded os.listdir')
        ):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root)
        self.assertEqual(seen_outer[0], pri.MAX_INSPECTED_CHILDREN + 1)

        # Inner overflow: 1 review dir with 20 files must stop inner scan at 4 entries
        # on the real pinned path without ever calling os.listdir or propose_edit.
        storage = self._reset_reviews_dir()
        crowded = _make_crowded_dir(storage / ('review-' + 'f' * 32), count=20)
        self.assertTrue(crowded.is_dir())

        seen_total = [0]
        counting_inner = _make_counting_scandir(real_scandir, seen_total)

        with mock.patch.object(pri.os, 'scandir', counting_inner), mock.patch.object(
            os, 'listdir', side_effect=AssertionError('must never call unbounded os.listdir')
        ), mock.patch.object(
            pri, 'propose_edit', side_effect=AssertionError('must not reach propose_edit')
        ):
            inv = pri.list_review_inventory(self.data_root)
        self.assertEqual(inv['invalid_count'], 1)
        self.assertEqual(inv['items'][0]['reason'], 'incomplete_or_extra_files')
        # 1 outer entry + 4 inner entries (stopped immediately on 4th file).
        self.assertEqual(seen_total[0], 5)

    def test_race_swap_and_add_entries_between_precheck_and_open_refused_boundedly(self):
        p = _make_proposal('race.py', 'v = 1\n', 'v = 2\n')
        real_open = os.open
        real_lstat = os.lstat
        real_read = os.read
        real_scandir = os.scandir

        with mock.patch.object(
            os, 'listdir', side_effect=AssertionError('must never call unbounded os.listdir')
        ):
            # 1) Add 50 extra files inside review_id between lstat precheck and os.open.
            storage = self._reset_reviews_dir()
            r1 = create_review_workspace(storage, p, request_id='1' * 32)
            rid1 = r1['review_id']
            seen = [0]
            triggered = [False]

            def add_entries_before_open(*args, **kwargs):
                if not triggered[0] and args and args[0] == rid1 and kwargs.get('dir_fd') is not None:
                    triggered[0] = True
                    for i in range(50):
                        extra = storage / rid1 / f'race_extra_{i:03d}.txt'
                        extra.write_bytes(b'x')
                        os.chmod(extra, 0o600)
                return real_open(*args, **kwargs)

            with mock.patch.object(
                pri.os, 'scandir', _make_counting_scandir(real_scandir, seen)
            ), mock.patch.object(pri.os, 'open', side_effect=add_entries_before_open):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            self.assertEqual(inv['valid_count'], 0)
            self.assertEqual(inv['invalid_count'], 1)
            self.assertEqual(inv['items'][0]['reason'], 'incomplete_or_extra_files')
            # 1 outer + 4 inner entries: bounded refusal, never scanned all 53 files.
            self.assertEqual(seen[0], 5)

            # 2) Swap review_id with another crowded 50-file dir between lstat precheck and os.open.
            storage = self._reset_reviews_dir()
            r2 = create_review_workspace(storage, p, request_id='2' * 32)
            rid2 = r2['review_id']
            crowded_swap = _make_crowded_dir(self.base / 'crowded-swap-dir', count=50)
            seen = [0]
            triggered = [False]

            def swap_dir_before_open(*args, **kwargs):
                if not triggered[0] and args and args[0] == rid2 and kwargs.get('dir_fd') is not None:
                    triggered[0] = True
                    os.rename(storage / rid2, self.base / 'orig-review-moved-2')
                    os.rename(crowded_swap, storage / rid2)
                return real_open(*args, **kwargs)

            with mock.patch.object(
                pri.os, 'scandir', _make_counting_scandir(real_scandir, seen)
            ), mock.patch.object(pri.os, 'open', side_effect=swap_dir_before_open):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            self.assertEqual(inv['valid_count'], 0)
            self.assertEqual(inv['invalid_count'], 1)
            self.assertEqual(inv['items'][0]['reason'], 'verification_failed')
            # Inode mismatch caught on fstat(child_fd): 0 inner entries scanned.
            self.assertEqual(seen[0], 1)

            # 3) Swap review_id with a symlink to a crowded outside dir between lstat and os.open.
            storage = self._reset_reviews_dir()
            r3 = create_review_workspace(storage, p, request_id='3' * 32)
            rid3 = r3['review_id']
            outside_crowded = _make_crowded_dir(self.base / 'outside-crowded-dir', count=50)
            seen = [0]
            triggered = [False]

            def swap_symlink_before_open(*args, **kwargs):
                if not triggered[0] and args and args[0] == rid3 and kwargs.get('dir_fd') is not None:
                    triggered[0] = True
                    os.rename(storage / rid3, self.base / 'orig-review-moved-3')
                    os.symlink(outside_crowded, storage / rid3)
                return real_open(*args, **kwargs)

            with mock.patch.object(
                pri.os, 'scandir', _make_counting_scandir(real_scandir, seen)
            ), mock.patch.object(pri.os, 'open', side_effect=swap_symlink_before_open):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            self.assertEqual(inv['valid_count'], 0)
            self.assertEqual(inv['invalid_count'], 1)
            self.assertEqual(inv['items'][0]['reason'], 'invalid_entry_type')
            self.assertEqual(seen[0], 1)

            # 4) Swap review_id with a crowded 50-file dir between outer scandir and child lstat.
            storage = self._reset_reviews_dir()
            r4 = create_review_workspace(storage, p, request_id='4' * 32)
            rid4 = r4['review_id']
            crowded_after_outer = _make_crowded_dir(self.base / 'crowded-after-outer', count=50)
            seen = [0]
            triggered = [False]

            def swap_before_lstat(*args, **kwargs):
                if not triggered[0] and args and args[0] == rid4 and kwargs.get('dir_fd') is not None:
                    triggered[0] = True
                    os.rename(storage / rid4, self.base / 'orig-review-moved-4')
                    os.rename(crowded_after_outer, storage / rid4)
                return real_lstat(*args, **kwargs)

            with mock.patch.object(
                pri.os, 'scandir', _make_counting_scandir(real_scandir, seen)
            ), mock.patch.object(pri.os, 'lstat', side_effect=swap_before_lstat):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            self.assertEqual(inv['valid_count'], 0)
            self.assertEqual(inv['invalid_count'], 1)
            self.assertEqual(inv['items'][0]['reason'], 'incomplete_or_extra_files')
            self.assertEqual(seen[0], 5)

            # 5) Replace `<data_root>/proposal-reviews` at the path level after storage_fd is pinned.
            storage = self._reset_reviews_dir()
            r5 = create_review_workspace(storage, p, request_id='5' * 32)
            rid5 = r5['review_id']
            decoy_storage = self.base / 'decoy-storage'
            decoy_storage.mkdir(mode=0o700)
            os.chmod(decoy_storage, 0o700)
            _make_crowded_dir(decoy_storage / rid5, count=50)
            triggered = [False]

            def swap_storage_path_after_pin(*args, **kwargs):
                if not triggered[0] and args and args[0] == rid5 and kwargs.get('dir_fd') is not None:
                    triggered[0] = True
                    os.rename(storage, self.base / 'pinned-storage-relocated')
                    os.rename(decoy_storage, storage)
                return real_open(*args, **kwargs)

            with mock.patch.object(pri.os, 'open', side_effect=swap_storage_path_after_pin):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            # Pinned storage_fd stayed on the verified original directory and never reopened by path.
            self.assertEqual(inv['valid_count'], 1)
            self.assertEqual(inv['invalid_count'], 0)
            self.assertEqual(inv['items'][0]['review_id'], rid5)

            # 6) Swap before.txt with symlink to outside secret between file lstat and os.open.
            storage = self._reset_reviews_dir()
            r6 = create_review_workspace(storage, p, request_id='6' * 32)
            rid6 = r6['review_id']
            outside_secret = self.base / 'race-outside-secret.txt'
            outside_secret.write_text('RACE_OUTSIDE_SECRET_NEVER_READ\n', encoding='utf-8')
            os.chmod(outside_secret, 0o600)
            triggered = [False]

            def swap_file_to_symlink(*args, **kwargs):
                if (
                    not triggered[0]
                    and args
                    and args[0] == 'before.txt'
                    and kwargs.get('dir_fd') is not None
                ):
                    triggered[0] = True
                    (storage / rid6 / 'before.txt').unlink()
                    (storage / rid6 / 'before.txt').symlink_to(outside_secret)
                return real_open(*args, **kwargs)

            with mock.patch.object(pri.os, 'open', side_effect=swap_file_to_symlink):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            self.assertEqual(inv['valid_count'], 0)
            self.assertEqual(inv['invalid_count'], 1)
            self.assertEqual(inv['items'][0]['reason'], 'invalid_entry_type')
            self.assertNotIn('RACE_OUTSIDE_SECRET_NEVER_READ', json.dumps(inv))

            # 7) Mutate before.txt mid-read between chunk 1 and chunk 2.
            storage = self._reset_reviews_dir()
            large_p = _make_multichunk_proposal('src/midread_race.py')
            r7 = create_review_workspace(storage, large_p, request_id='7' * 32)
            rid7 = r7['review_id']
            triggered = [False]

            def mutate_mid_read(fd, n):
                chunk = real_read(fd, n)
                if not triggered[0] and len(chunk) == pri.READ_CHUNK_BYTES:
                    triggered[0] = True
                    with open(storage / rid7 / 'before.txt', 'ab') as fh:
                        fh.write(b'# concurrent mutation\n')
                return chunk

            with mock.patch.object(pri.os, 'read', side_effect=mutate_mid_read):
                inv = pri.list_review_inventory(self.data_root)

            self.assertTrue(triggered[0])
            self.assertEqual(inv['valid_count'], 0)
            self.assertEqual(inv['invalid_count'], 1)
            self.assertEqual(inv['items'][0]['reason'], 'verification_failed')

    def test_precancel_midread_midhash_and_deadline_expiry_abort_inventory(self):
        storage = self._reviews_dir()
        large_p = _make_multichunk_proposal('src/cancel_chunks.py')
        r1 = create_review_workspace(storage, large_p, request_id='a' * 32)
        r2 = create_review_workspace(storage, large_p, request_id='b' * 32)

        # Pre-cancel
        cancel = threading.Event()
        cancel.set()
        with self.assertRaises(ProjectReadError):
            pri.list_review_inventory(self.data_root, cancel=cancel)

        # Mid-read cancel inside chunked file read (stops right after chunk 1).
        mid_read_cancel = threading.Event()
        read_chunks = [0]
        real_read = os.read

        def cancel_after_first_read_chunk(fd, n):
            chunk = real_read(fd, n)
            read_chunks[0] += 1
            if read_chunks[0] == 1:
                mid_read_cancel.set()
            return chunk

        with mock.patch.object(pri.os, 'read', side_effect=cancel_after_first_read_chunk):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root, cancel=mid_read_cancel)
        self.assertEqual(read_chunks[0], 1)

        # Mid-read deadline expiry inside chunked file read.
        now_read = [100.0]
        read_chunks_dl = [0]

        def expire_after_first_read_chunk(fd, n):
            chunk = real_read(fd, n)
            read_chunks_dl[0] += 1
            if read_chunks_dl[0] == 1:
                now_read[0] = 105.0
            return chunk

        with mock.patch.object(pri.time, 'monotonic', side_effect=lambda: now_read[0]), mock.patch.object(
            pri.os, 'read', side_effect=expire_after_first_read_chunk
        ):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root, timeout_seconds=1.0)
        self.assertEqual(read_chunks_dl[0], 1)

        # Mid-hash cancel inside chunked SHA-256 hashing (stops right after chunk 1).
        mid_hash_cancel = threading.Event()
        hash_chunks = [0]
        real_sha256 = hashlib.sha256

        class CancelingSha256:
            def __init__(self):
                self._h = real_sha256()

            def update(self, data):
                self._h.update(data)
                hash_chunks[0] += 1
                if hash_chunks[0] == 1:
                    mid_hash_cancel.set()

            def hexdigest(self):
                return self._h.hexdigest()

        with mock.patch.object(pri.hashlib, 'sha256', side_effect=CancelingSha256), mock.patch.object(
            pri, 'propose_edit', side_effect=AssertionError('must abort before propose_edit')
        ):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root, cancel=mid_hash_cancel)
        self.assertEqual(hash_chunks[0], 1)

        # Mid-hash deadline expiry inside chunked SHA-256 hashing.
        now_hash = [200.0]
        hash_chunks_dl = [0]

        class ExpiringSha256:
            def __init__(self):
                self._h = real_sha256()

            def update(self, data):
                self._h.update(data)
                hash_chunks_dl[0] += 1
                if hash_chunks_dl[0] == 1:
                    now_hash[0] = 205.0

            def hexdigest(self):
                return self._h.hexdigest()

        with mock.patch.object(pri.time, 'monotonic', side_effect=lambda: now_hash[0]), mock.patch.object(
            pri.hashlib, 'sha256', side_effect=ExpiringSha256
        ):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root, timeout_seconds=1.0)
        self.assertEqual(hash_chunks_dl[0], 1)

        # Mid-inventory cancel between reviews on the real pinned path (no inspector mock).
        between_cancel = threading.Event()
        ordered_ids = sorted([r1['review_id'], r2['review_id']])
        second_id = ordered_ids[1]
        real_open = os.open

        def cancel_on_second_review(*args, **kwargs):
            if args and args[0] == second_id and kwargs.get('dir_fd') is not None:
                between_cancel.set()
            return real_open(*args, **kwargs)

        with mock.patch.object(pri.os, 'open', side_effect=cancel_on_second_review):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root, cancel=between_cancel)

    def test_serialized_output_size_cap_enforced(self):
        storage = self._reviews_dir()
        p1 = _make_proposal('src/first.py', 'a = 1\n', 'a = 2\n')
        p2 = _make_proposal('src/second.py', 'b = 1\n', 'b = 2\n')
        create_review_workspace(storage, p1, request_id='a' * 32)
        create_review_workspace(storage, p2, request_id='b' * 32)

        inv = pri.list_review_inventory(self.data_root)
        serialized = pri.serialize_inventory_result(inv)
        exact_len = len(serialized.encode('ascii'))
        self.assertLessEqual(exact_len, pri.MAX_OUTPUT_BYTES)

        # Exact boundary passes; 1 byte tighter fails closed.
        self.assertEqual(pri.serialize_inventory_result(inv, max_bytes=exact_len), serialized)
        with self.assertRaises(ProjectReadError):
            pri.serialize_inventory_result(inv, max_bytes=exact_len - 1)

        valid_frame = (json.dumps({'schema': pri.REQUEST_SCHEMA}) + '\n').encode('utf-8')
        with mock.patch.object(pri, 'MAX_OUTPUT_BYTES', exact_len - 1):
            with self.assertRaises(ProjectReadError):
                pri.list_review_inventory(self.data_root)
            with self.assertRaises(ProjectReadError):
                pri.inventory_request(self.data_root, valid_frame)

        # CLI main() fails closed with error JSON when serialized output exceeds cap.
        fake_stdin = io.TextIOWrapper(io.BytesIO(valid_frame), encoding='utf-8')
        fake_stdout = io.StringIO()
        with mock.patch.object(pri, 'MAX_OUTPUT_BYTES', 200), mock.patch.object(
            pri.signal, 'signal'
        ), mock.patch.object(
            sys, 'argv', ['project_review_inventory.py', '--owner-data', str(self.data_root)]
        ), mock.patch.object(sys, 'stdin', fake_stdin), mock.patch.object(
            sys, 'stdout', fake_stdout
        ):
            rc = pri.main()
        self.assertEqual(rc, 1)
        err_payload = json.loads(fake_stdout.getvalue())
        self.assertFalse(err_payload['applied'])
        self.assertFalse(err_payload['source_contents_exposed'])
        self.assertIn('error', err_payload)

    def test_inventory_performs_no_writes_deletes_or_cleanup_on_partial_or_valid_reviews(self):
        storage = self._reviews_dir()
        p = _make_proposal('keep.py', 'x = 1\n', 'x = 2\n')
        r1 = create_review_workspace(storage, p, request_id='1' * 32)
        r2 = create_review_workspace(storage, p, request_id='2' * 32)
        (storage / r2['review_id'] / 'review.json').unlink()

        def snapshot_tree():
            state = {}
            for entry in sorted(storage.iterdir()):
                files = {}
                for child in sorted(entry.iterdir()):
                    st = child.stat()
                    files[child.name] = (child.read_bytes(), stat.S_IMODE(st.st_mode))
                state[entry.name] = files
            return state

        before_state = snapshot_tree()
        inv = pri.list_review_inventory(self.data_root)
        after_state = snapshot_tree()

        self.assertEqual(inv['valid_count'], 1)
        self.assertEqual(inv['invalid_count'], 1)
        self.assertEqual(before_state, after_state)


if __name__ == '__main__':
    unittest.main()
