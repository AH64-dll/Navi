"""Actual owner-only private review apply route; synthetic project only."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

import test_assistant

sys.path.insert(0, str(test_assistant.ROOT / 'workers'))
from project_edit_reviews import proposal_digest
from project_edits import propose_edit
from project_workspace import create_review_workspace, owner_review_storage


class ReviewApplyOwnerTests(test_assistant.AssistantTests):
    def setUp(self):
        self.old_apply = os.environ.get('NAVI_PROJECT_REVIEW_APPLY')
        os.environ['NAVI_PROJECT_REVIEW_APPLY'] = '1'
        self.oldtemp = tempfile.tempdir
        tempfile.tempdir = str(Path.home())
        try:
            super().setUp()
        finally:
            tempfile.tempdir = self.oldtemp
        self.call('assistant_scope', {'project_id': self.project()['id']})

    def tearDown(self):
        super().tearDown()
        if self.old_apply is None:
            os.environ.pop('NAVI_PROJECT_REVIEW_APPLY', None)
        else:
            os.environ['NAVI_PROJECT_REVIEW_APPLY'] = self.old_apply

    def _confirm(self, review_id):
        return f'apply private review {review_id}'

    def _params(self, review_id, digest, *, confirm=None):
        return {
            'project_id': self.project()['id'],
            'review_id': review_id,
            'proposal_digest': digest,
            'confirm': self._confirm(review_id) if confirm is None else confirm,
        }

    def _seed(self, rel, before, after, request_id):
        root = Path(self.project()['path'])
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(before, encoding='utf-8')
        os.chmod(target, 0o600)
        sha = hashlib.sha256(before.encode('utf-8')).hexdigest()
        proposal = propose_edit(rel, sha, before, after)
        created = create_review_workspace(owner_review_storage(self.data), proposal, request_id=request_id)
        return target, created['review_id'], proposal_digest(proposal)

    def test_default_off(self):
        target, rid, digest = self._seed('src/a.py', 'x = 1\n', 'x = 2\n', '1' * 32)
        self.process.terminate()
        self.process.wait(3)
        env = dict(self.environment)
        env.pop('NAVI_PROJECT_REVIEW_APPLY', None)
        self.process = subprocess.Popen(
            [str(test_assistant.BINARY), 'serve'], env=env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(0.15)
        self.call('assistant_scope', {'project_id': self.project()['id']})
        res = self.call('pi_review_apply', self._params(rid, digest))
        self.assertFalse(res['ok'], res)
        self.assertIn('disabled', res['error'].lower())
        self.assertEqual(target.read_text(encoding='utf-8'), 'x = 1\n')

    def test_strict_params_and_confirm_change_nothing(self):
        target, rid, digest = self._seed('src/b.py', 'a = 1\n', 'a = 2\n', '2' * 32)
        good = self._params(rid, digest)
        bad_payloads = [
            {}, {k: v for k, v in good.items() if k != 'confirm'},
            {**good, 'confirm': 'yes'}, {**good, 'confirm': f'delete private review {rid}'},
            {**good, 'confirm': f' {self._confirm(rid)}'}, {**good, 'proposal_digest': 'A' * 64},
            {**good, 'review_id': '../' + rid}, {**good, 'project_id': ''},
            {**good, 'path': 'src/b.py'}, {**good, 'force': True},
        ]
        for bad in bad_payloads:
            res = self.call('pi_review_apply', bad)
            self.assertFalse(res['ok'], bad)
            self.assertNotIn(str(self.data), json.dumps(res))
            self.assertNotIn(str(target), json.dumps(res))
        self.assertEqual(target.read_text(encoding='utf-8'), 'a = 1\n')

    def test_apply_replaces_target_and_keeps_review_copy(self):
        target, rid, digest = self._seed('src/c.py', 'v = 1\n', 'v = 2\n', '3' * 32)
        res = self.call('pi_review_apply', self._params(rid, digest))
        self.assertTrue(res['ok'], res)
        self.assertEqual(res['result'], {
            'schema': 'navi-review-apply-v1', 'applied': True, 'review_id': rid})
        self.assertEqual(target.read_text(encoding='utf-8'), 'v = 2\n')
        self.assertTrue((owner_review_storage(self.data) / rid).is_dir())
        self.assertNotIn(str(self.data), json.dumps(res))

    def test_digest_mismatch_and_changed_target_leave_file_untouched(self):
        target, rid, digest = self._seed('src/d.py', 'k = 1\n', 'k = 2\n', '4' * 32)
        wrong = ('0' if digest[0] != '0' else '1') * 64
        res = self.call('pi_review_apply', self._params(rid, wrong))
        self.assertTrue(res['ok'], res)
        self.assertEqual(res['result']['applied'], False)
        self.assertEqual(res['result']['reason'], 'digest_mismatch')
        self.assertEqual(target.read_text(encoding='utf-8'), 'k = 1\n')
        target.write_text('k = 99\n', encoding='utf-8')
        os.chmod(target, 0o600)
        res = self.call('pi_review_apply', self._params(rid, digest))
        self.assertTrue(res['ok'], res)
        self.assertEqual(res['result']['reason'], 'before_mismatch')
        self.assertEqual(target.read_text(encoding='utf-8'), 'k = 99\n')


if __name__ == '__main__':
    unittest.main()
