"""Actual owner-only read-only private review inventory route; synthetic project only."""
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
from project_edits import propose_edit
from project_workspace import create_review_workspace, owner_review_storage


class ReviewInventoryOwnerTests(test_assistant.AssistantTests):
    def setUp(self):
        self.old = os.environ.get('NAVI_PROJECT_REVIEW_INVENTORY')
        os.environ['NAVI_PROJECT_REVIEW_INVENTORY'] = '1'
        self.oldtemp = tempfile.tempdir
        tempfile.tempdir = str(Path.home())
        try:
            super().setUp()
        finally:
            tempfile.tempdir = self.oldtemp
        self.call('assistant_scope', {'project_id': self.project()['id']})

    def tearDown(self):
        super().tearDown()
        if self.old is None:
            os.environ.pop('NAVI_PROJECT_REVIEW_INVENTORY', None)
        else:
            os.environ['NAVI_PROJECT_REVIEW_INVENTORY'] = self.old

    def params(self):
        return {'project_id': self.project()['id']}

    def _seed_review(self, storage, path, before, after, request_id):
        sha = hashlib.sha256(before.encode('utf-8')).hexdigest()
        proposal = propose_edit(path, sha, before, after)
        return create_review_workspace(storage, proposal, request_id=request_id)

    def _snapshot_reviews_tree(self, storage):
        state = {}
        for entry in sorted(storage.iterdir()):
            files = {}
            for child in sorted(entry.iterdir()):
                st = child.stat()
                files[child.name] = (child.read_bytes(), stat.S_IMODE(st.st_mode))
            state[entry.name] = files
        return state

    def _assert_no_creation_wording(self, error_text):
        lower = error_text.lower()
        for forbidden in ('same request id', 'recover', 'partial state', 'create'):
            self.assertNotIn(forbidden, lower)

    def test_default_off(self):
        self.process.terminate()
        self.process.wait(3)
        env = dict(self.environment)
        env.pop('NAVI_PROJECT_REVIEW_INVENTORY', None)
        self.process = subprocess.Popen(
            [str(test_assistant.BINARY), 'serve'],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        time.sleep(0.15)
        self.call('assistant_scope', {'project_id': self.project()['id']})
        result = self.call('pi_review_inventory', self.params())
        self.assertFalse(result['ok'])
        self.assertIn('disabled', result['error'])
        self._assert_no_creation_wording(result['error'])

    def test_strict_params_and_no_extra_fields(self):
        for bad in (
            {},
            {'project_id': ''},
            {'project_id': 123},
            {'project_id': 'nonexistent-project'},
        ):
            res = self.call('pi_review_inventory', bad)
            self.assertFalse(res['ok'], bad)
            self._assert_no_creation_wording(res['error'])

        for field, value in (
            ('root', '/'),
            ('storage', '/'),
            ('path', 'src/app.py'),
            ('command', 'ls'),
            ('delete', True),
            ('request_id', 'a' * 32),
            ('preview', {}),
        ):
            p = self.params()
            p[field] = value
            res = self.call('pi_review_inventory', p)
            self.assertFalse(res['ok'], field)
            self._assert_no_creation_wording(res['error'])

        # Unselected project must be refused.
        other = self.folder / 'other-unselected'
        other.mkdir()
        added = self.call('project_add', {'name': 'Other', 'path': str(other)})
        other_id = added['result']['project']['id']
        res = self.call('pi_review_inventory', {'project_id': other_id})
        self.assertFalse(res['ok'], res)
        self._assert_no_creation_wording(res['error'])

    def test_empty_storage_returns_metadata_without_creating_directory(self):
        reviews_dir = self.data / 'proposal-reviews'
        self.assertFalse(reviews_dir.exists())

        res = self.call('pi_review_inventory', self.params())
        self.assertTrue(res['ok'], res)
        inv = res['result']
        self.assertEqual(inv['schema'], 'navi-review-inventory-v1')
        self.assertFalse(inv['applied'])
        self.assertFalse(inv['source_contents_exposed'])
        self.assertFalse(inv['storage_present'])
        self.assertEqual(inv['inspected_count'], 0)
        self.assertEqual(inv['valid_count'], 0)
        self.assertEqual(inv['invalid_count'], 0)
        self.assertEqual(inv['items'], [])
        self.assertNotIn('storage_path', inv)
        self.assertNotIn(str(self.data), json.dumps(res))
        self.assertFalse(reviews_dir.exists())
        self.assertEqual(self.posts('/sessions'), [])

    def test_one_valid_and_one_invalid_review_metadata_only_no_writes(self):
        storage = owner_review_storage(self.data)
        before_ok = 'SECRET_BEFORE_CONTENT = "alpha"\n'
        after_ok = 'SECRET_AFTER_CONTENT = "beta"\n'
        valid_r = self._seed_review(storage, 'src/demo.py', before_ok, after_ok, '1' * 32)
        invalid_r = self._seed_review(
            storage,
            'src/partial.py',
            'SECRET_PARTIAL_BEFORE = 1\n',
            'SECRET_PARTIAL_AFTER = 2\n',
            '2' * 32,
        )
        (storage / invalid_r['review_id'] / 'review.json').unlink()

        before_tree = self._snapshot_reviews_tree(storage)
        res = self.call('pi_review_inventory', self.params())
        after_tree = self._snapshot_reviews_tree(storage)

        self.assertEqual(before_tree, after_tree)
        self.assertTrue(res['ok'], res)
        inv = res['result']
        self.assertEqual(inv['schema'], 'navi-review-inventory-v1')
        self.assertFalse(inv['applied'])
        self.assertFalse(inv['source_contents_exposed'])
        self.assertTrue(inv['storage_present'])
        self.assertEqual(inv['inspected_count'], 2)
        self.assertEqual(inv['valid_count'], 1)
        self.assertEqual(inv['invalid_count'], 1)
        self.assertNotIn('storage_path', inv)

        by_id = {item['review_id']: item for item in inv['items']}
        self.assertEqual(set(by_id), {valid_r['review_id'], invalid_r['review_id']})

        v_item = by_id[valid_r['review_id']]
        self.assertTrue(v_item['valid'])
        self.assertEqual(v_item['status'], 'valid')
        self.assertFalse(v_item['applied'])
        self.assertTrue(v_item['requires_fresh_validation'])
        self.assertEqual(v_item['original_path'], 'src/demo.py')
        self.assertEqual(v_item['before_bytes'], len(before_ok.encode('utf-8')))
        self.assertEqual(v_item['after_bytes'], len(after_ok.encode('utf-8')))
        self.assertEqual(v_item['files'], ['before.txt', 'proposed-after.txt', 'review.json'])
        for forbidden in ('before', 'after', 'proposal', 'created', 'storage_path', 'request_id'):
            self.assertNotIn(forbidden, v_item)

        bad_item = by_id[invalid_r['review_id']]
        self.assertEqual(
            bad_item,
            {
                'review_id': invalid_r['review_id'],
                'valid': False,
                'status': 'invalid',
                'reason': 'incomplete_or_extra_files',
                'applied': False,
            },
        )

        raw = json.dumps(res)
        self.assertLessEqual(len(raw.encode('utf-8')), 65536)
        for secret in (
            'SECRET_BEFORE_CONTENT',
            'SECRET_AFTER_CONTENT',
            'SECRET_PARTIAL_BEFORE',
            'SECRET_PARTIAL_AFTER',
            str(self.data),
            'storage_path',
        ):
            self.assertNotIn(secret, raw)
        self.assertEqual(self.posts('/sessions'), [])

    def _start_delayed_inventory_daemon(self):
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
        marker = self.folder / 'inventory-started'
        worker = app / 'workers/project_review_inventory.py'
        text = worker.read_text(encoding='utf-8')
        target = '        sys.stdout.write(encoded)'
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
        self.fail('inventory helper never started')

    def _assert_reaped(self, pid):
        for _ in range(100):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.02)
        self.fail('inventory helper not reaped')

    def test_project_switch_aborts_and_reaps_without_writing(self):
        marker = self._start_delayed_inventory_daemon()
        p = self.params()
        result = {}
        task = threading.Thread(
            target=lambda: result.update(value=self.call('pi_review_inventory', p))
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
        self._assert_no_creation_wording(result['value']['error'])
        self._assert_reaped(pid)
        self.assertFalse((self.data / 'proposal-reviews').exists())

    def test_owner_eof_aborts_and_reaps_without_writing(self):
        marker = self._start_delayed_inventory_daemon()
        p = self.params()
        conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        conn.connect(str(self.runtime / 'station.sock'))
        conn.sendall(
            (json.dumps({'op': 'pi_review_inventory', 'params': p}) + '\n').encode('utf-8')
        )
        pid = self._wait_started(marker)
        conn.close()
        self._assert_reaped(pid)
        self.assertFalse((self.data / 'proposal-reviews').exists())


if __name__ == '__main__':
    suite = unittest.TestSuite(
        ReviewInventoryOwnerTests(n)
        for n in ReviewInventoryOwnerTests.__dict__
        if n.startswith('test_')
    )
    raise SystemExit(not unittest.TextTestRunner().run(suite).wasSuccessful())