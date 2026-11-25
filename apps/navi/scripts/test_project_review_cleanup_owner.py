"""Actual owner-only private review cleanup route; synthetic project only."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest

import test_assistant

sys.path.insert(0, str(test_assistant.ROOT / 'workers'))
from project_edit_reviews import proposal_digest
from project_edits import propose_edit
from project_workspace import create_review_workspace, owner_review_storage


class ReviewCleanupOwnerTests(test_assistant.AssistantTests):
    def setUp(self):
        self.old_cleanup = os.environ.get('NAVI_PROJECT_REVIEW_CLEANUP')
        self.old_inventory = os.environ.get('NAVI_PROJECT_REVIEW_INVENTORY')
        os.environ['NAVI_PROJECT_REVIEW_CLEANUP'] = '1'
        os.environ.pop('NAVI_PROJECT_REVIEW_INVENTORY', None)
        self.oldtemp = tempfile.tempdir
        tempfile.tempdir = str(Path.home())
        try:
            super().setUp()
        finally:
            tempfile.tempdir = self.oldtemp
        self.call('assistant_scope', {'project_id': self.project()['id']})

    def tearDown(self):
        super().tearDown()
        for key, old in (
            ('NAVI_PROJECT_REVIEW_CLEANUP', self.old_cleanup),
            ('NAVI_PROJECT_REVIEW_INVENTORY', self.old_inventory),
        ):
            if old is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = old

    def _confirm(self, review_id):
        return f'delete private review {review_id}'

    def _params(self, review_id, digest, *, confirm=None, project_id=None):
        return {
            'project_id': self.project()['id'] if project_id is None else project_id,
            'review_id': review_id,
            'proposal_digest': digest,
            'confirm': self._confirm(review_id) if confirm is None else confirm,
        }

    def _seed_review(self, storage, path, before, after, request_id):
        sha = hashlib.sha256(before.encode('utf-8')).hexdigest()
        proposal = propose_edit(path, sha, before, after)
        created = create_review_workspace(storage, proposal, request_id=request_id)
        return created['review_id'], proposal_digest(proposal)

    def _snapshot_reviews_tree(self, storage):
        state = {}
        for entry in sorted(storage.iterdir()):
            files = {}
            for child in sorted(entry.iterdir()):
                st = child.stat()
                files[child.name] = (child.read_bytes(), stat.S_IMODE(st.st_mode))
            state[entry.name] = files
        return state

    def _start_delayed_cleanup_daemon(self):
        self.process.terminate()
        self.process.wait(3)
        app = self.folder / 'fixed-app'
        if app.exists():
            shutil.rmtree(app)
        shutil.copytree(
            test_assistant.ROOT,
            app,
            ignore=shutil.ignore_patterns('target', '__pycache__'),
        )
        marker = self.folder / 'cleanup-started'
        worker = app / 'workers/project_review_cleanup.py'
        text = worker.read_text(encoding='utf-8')
        target = '        result = cleanup_review_workspace('
        self.assertIn(target, text)
        injected = (
            '        Path(' + repr(str(marker)) + ').write_text(str(os.getpid()))\n'
            '        time.sleep(3)\n'
            + target
        )
        worker.write_text(text.replace(target, injected, 1), encoding='utf-8')
        env = {**self.environment, 'NAVI_APP_DIR': str(app)}
        self.process = subprocess.Popen(
            [str(test_assistant.BINARY), 'serve'],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        time.sleep(0.15)
        self.call('assistant_scope', {'project_id': self.project()['id']})
        return marker

    def _wait_started(self, marker):
        for _ in range(100):
            if marker.exists():
                return int(marker.read_text(encoding='utf-8'))
            time.sleep(0.02)
        self.fail('cleanup helper never started')

    def _assert_reaped(self, pid):
        for _ in range(100):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.02)
        self.fail('cleanup helper not reaped')

    def test_default_off_and_separate_from_inventory_flag(self):
        storage = owner_review_storage(self.data)
        rid, digest = self._seed_review(storage, 'src/a.py', 'x = 1\n', 'x = 2\n', '1' * 32)
        before_tree = self._snapshot_reviews_tree(storage)

        self.process.terminate()
        self.process.wait(3)
        env = dict(self.environment)
        env.pop('NAVI_PROJECT_REVIEW_CLEANUP', None)
        env['NAVI_PROJECT_REVIEW_INVENTORY'] = '1'
        self.process = subprocess.Popen(
            [str(test_assistant.BINARY), 'serve'],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        time.sleep(0.15)
        self.call('assistant_scope', {'project_id': self.project()['id']})

        res = self.call('pi_review_cleanup', self._params(rid, digest))
        self.assertFalse(res['ok'], res)
        self.assertIn('disabled', res['error'].lower())
        self.assertNotIn(str(self.data), json.dumps(res))
        self.assertEqual(self._snapshot_reviews_tree(storage), before_tree)

    def test_strict_params_extra_fields_and_wrong_confirm(self):
        storage = owner_review_storage(self.data)
        rid, digest = self._seed_review(storage, 'src/b.py', 'a = 1\n', 'a = 2\n', '2' * 32)
        before_tree = self._snapshot_reviews_tree(storage)
        good = self._params(rid, digest)

        bad_payloads = [
            {},
            {k: v for k, v in good.items() if k != 'confirm'},
            {k: v for k, v in good.items() if k != 'proposal_digest'},
            {k: v for k, v in good.items() if k != 'review_id'},
            {**good, 'project_id': ''},
            {**good, 'project_id': 123},
            {**good, 'project_id': 'nonexistent-project'},
            {**good, 'review_id': '../' + rid, 'confirm': self._confirm('../' + rid)},
            {**good, 'review_id': 'review-' + 'A' * 32, 'confirm': self._confirm('review-' + 'A' * 32)},
            {**good, 'proposal_digest': 'A' * 64},
            {**good, 'proposal_digest': 'a' * 63},
            {**good, 'confirm': True},
            {**good, 'confirm': ''},
            {**good, 'confirm': 'yes'},
            {**good, 'confirm': 'delete private review'},
            {**good, 'confirm': self._confirm('review-' + '9' * 32)},
            {**good, 'confirm': f'DELETE PRIVATE REVIEW {rid}'},
            {**good, 'confirm': f' {self._confirm(rid)}'},
            {**good, 'confirm': f'{self._confirm(rid)} '},
        ]
        for bad in bad_payloads:
            res = self.call('pi_review_cleanup', bad)
            self.assertFalse(res['ok'], bad)
            self.assertNotIn(str(self.data), json.dumps(res))

        for extra_key, extra_val in (
            ('root', '/'),
            ('storage', '/'),
            ('path', 'src/b.py'),
            ('command', 'rm'),
            ('force', True),
            ('standing_approval', True),
        ):
            res = self.call('pi_review_cleanup', {**good, extra_key: extra_val})
            self.assertFalse(res['ok'], extra_key)
            self.assertNotIn(str(self.data), json.dumps(res))

        # Unselected project must be refused even with valid confirm.
        other = self.folder / 'other-unselected'
        other.mkdir()
        other_id = self.call('project_add', {'name': 'Other', 'path': str(other)})['result']['project']['id']
        res = self.call('pi_review_cleanup', self._params(rid, digest, project_id=other_id))
        self.assertFalse(res['ok'], res)
        self.assertEqual(self._snapshot_reviews_tree(storage), before_tree)

    def test_digest_mismatch_refused_with_nothing_deleted(self):
        storage = owner_review_storage(self.data)
        rid, digest = self._seed_review(
            storage,
            'src/secret_mismatch.py',
            'SECRET_BEFORE = "alpha"\n',
            'SECRET_AFTER = "beta"\n',
            '3' * 32,
        )
        before_tree = self._snapshot_reviews_tree(storage)
        wrong_digest = ('0' if digest[0] != '0' else '1') * 64

        res = self.call('pi_review_cleanup', self._params(rid, wrong_digest))
        self.assertTrue(res['ok'], res)
        self.assertEqual(
            res['result'],
            {
                'schema': 'navi-review-cleanup-v1',
                'deleted': False,
                'review_id': rid,
                'reason': 'digest_mismatch',
            },
        )
        raw = json.dumps(res)
        for forbidden in (str(self.data), 'SECRET_BEFORE', 'SECRET_AFTER', 'secret_mismatch'):
            self.assertNotIn(forbidden, raw)
        self.assertEqual(self._snapshot_reviews_tree(storage), before_tree)
        self.assertEqual(self.posts('/sessions'), [])

    def test_valid_delete_removes_only_target_review_and_no_project_files(self):
        proj_root = Path(self.project()['path'])
        proj_file = proj_root / 'src' / 'demo.py'
        proj_file.parent.mkdir(parents=True, exist_ok=True)
        orig_text = 'SECRET_PROJECT_FILE = "keep"\n'
        proj_file.write_text(orig_text, encoding='utf-8')
        proj_ino = proj_file.stat().st_ino

        storage = owner_review_storage(self.data)
        target_rid, target_digest = self._seed_review(
            storage, 'src/demo.py', orig_text, 'SECRET_PROJECT_FILE = "new"\n', '4' * 32
        )
        sibling_rid, _sibling_digest = self._seed_review(
            storage, 'src/other.py', 'KEEP = 1\n', 'KEEP = 2\n', '5' * 32
        )
        sibling_before = self._snapshot_reviews_tree(storage)[sibling_rid]

        res = self.call('pi_review_cleanup', self._params(target_rid, target_digest))
        self.assertTrue(res['ok'], res)
        self.assertEqual(
            res['result'],
            {
                'schema': 'navi-review-cleanup-v1',
                'deleted': True,
                'review_id': target_rid,
            },
        )
        raw = json.dumps(res)
        for forbidden in (
            str(self.data),
            str(proj_root),
            'src/demo.py',
            'SECRET_PROJECT_FILE',
            'storage_path',
        ):
            self.assertNotIn(forbidden, raw)
        self.assertFalse((storage / target_rid).exists())
        self.assertEqual(self._snapshot_reviews_tree(storage), {sibling_rid: sibling_before})
        self.assertEqual(proj_file.stat().st_ino, proj_ino)
        self.assertEqual(proj_file.read_text(encoding='utf-8'), orig_text)
        self.assertEqual(self.posts('/sessions'), [])

    def test_already_gone_is_idempotent(self):
        reviews_dir = self.data / 'proposal-reviews'
        self.assertFalse(reviews_dir.exists())
        missing_rid = 'review-' + '6' * 32
        res0 = self.call('pi_review_cleanup', self._params(missing_rid, 'a' * 64))
        self.assertTrue(res0['ok'], res0)
        self.assertEqual(
            res0['result'],
            {
                'schema': 'navi-review-cleanup-v1',
                'deleted': False,
                'review_id': missing_rid,
                'reason': 'not_found',
            },
        )
        self.assertFalse(reviews_dir.exists())

        storage = owner_review_storage(self.data)
        rid, digest = self._seed_review(storage, 'src/once.py', 'v = 1\n', 'v = 2\n', '7' * 32)
        first = self.call('pi_review_cleanup', self._params(rid, digest))
        self.assertTrue(first['ok'] and first['result']['deleted'], first)
        second = self.call('pi_review_cleanup', self._params(rid, digest))
        self.assertTrue(second['ok'], second)
        self.assertEqual(
            second['result'],
            {
                'schema': 'navi-review-cleanup-v1',
                'deleted': False,
                'review_id': rid,
                'reason': 'not_found',
            },
        )

    def test_project_switch_aborts_reaps_and_reports_unknown_outcome(self):
        storage = owner_review_storage(self.data)
        rid, digest = self._seed_review(storage, 'src/sw.py', 's = 1\n', 's = 2\n', '8' * 32)
        marker = self._start_delayed_cleanup_daemon()
        p = self._params(rid, digest)
        result = {}
        task = threading.Thread(
            target=lambda: result.update(value=self.call('pi_review_cleanup', p))
        )
        task.start()
        pid = self._wait_started(marker)

        other = self.folder / 'other-switch'
        other.mkdir()
        row = self.call('project_add', {'name': 'OtherSwitch', 'path': str(other)})
        self.call('assistant_scope', {'project_id': row['result']['project']['id']})

        task.join(2.5)
        self.assertFalse(task.is_alive())
        self.assertFalse(result['value']['ok'], result)
        err = result['value']['error'].lower()
        self.assertIn('unknown', err)
        self.assertIn('inventory', err)
        self.assertNotIn('deleted', err)
        self.assertNotIn(str(self.data), result['value']['error'])
        self._assert_reaped(pid)
        self.assertTrue((storage / rid).is_dir())

    def test_owner_eof_aborts_and_reaps_without_zombie(self):
        storage = owner_review_storage(self.data)
        rid, digest = self._seed_review(storage, 'src/eof.py', 'e = 1\n', 'e = 2\n', '9' * 32)
        marker = self._start_delayed_cleanup_daemon()
        p = self._params(rid, digest)

        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.connect(str(self.runtime / 'station.sock'))
        conn.sendall(
            (json.dumps({'op': 'pi_review_cleanup', 'params': p}) + '\n').encode('utf-8')
        )
        pid = self._wait_started(marker)
        conn.close()
        self._assert_reaped(pid)
        self.assertTrue((storage / rid).is_dir())

        # Mutex must be released after abort so a subsequent request is not stuck.
        marker.unlink()
        wrong = self._params(rid, '0' * 64, confirm='wrong')
        follow = self.call('pi_review_cleanup', wrong)
        self.assertFalse(follow['ok'])
        self.assertNotIn('already active', follow['error'].lower())


if __name__ == '__main__':
    suite = unittest.TestSuite(
        ReviewCleanupOwnerTests(n)
        for n in ReviewCleanupOwnerTests.__dict__
        if n.startswith('test_')
    )
    raise SystemExit(not unittest.TextTestRunner().run(suite).wasSuccessful())