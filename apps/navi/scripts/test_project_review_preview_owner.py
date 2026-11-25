"""Actual owner-only private review preview route; synthetic project only."""
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


class ReviewPreviewOwnerTests(test_assistant.AssistantTests):
    def setUp(self):
        self.old_flag = os.environ.get('NAVI_PROJECT_REVIEW_PREVIEW')
        os.environ['NAVI_PROJECT_REVIEW_PREVIEW'] = '1'
        self.oldtemp = tempfile.tempdir
        tempfile.tempdir = str(Path.home())
        try:
            super().setUp()
        finally:
            tempfile.tempdir = self.oldtemp
        self.call('assistant_scope', {'project_id': self.project()['id']})

    def tearDown(self):
        super().tearDown()
        if self.old_flag is None:
            os.environ.pop('NAVI_PROJECT_REVIEW_PREVIEW', None)
        else:
            os.environ['NAVI_PROJECT_REVIEW_PREVIEW'] = self.old_flag

    def _params(self, review_id, digest):
        return {'project_id': self.project()['id'], 'review_id': review_id, 'proposal_digest': digest}

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
        env.pop('NAVI_PROJECT_REVIEW_PREVIEW', None)
        self.process = subprocess.Popen(
            [str(test_assistant.BINARY), 'serve'], env=env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(0.15)
        self.call('assistant_scope', {'project_id': self.project()['id']})
        res = self.call('pi_review_preview', self._params(rid, digest))
        self.assertFalse(res['ok'], res)
        self.assertIn('disabled', res['error'].lower())

    def test_strict_params(self):
        target, rid, digest = self._seed('src/b.py', 'a = 1\n', 'a = 2\n', '2' * 32)
        good = self._params(rid, digest)
        for bad in [{}, {k: v for k, v in good.items() if k != 'proposal_digest'},
                    {**good, 'proposal_digest': 'A' * 64}, {**good, 'review_id': '../' + rid},
                    {**good, 'project_id': ''}, {**good, 'path': 'src/b.py'}]:
            res = self.call('pi_review_preview', bad)
            self.assertFalse(res['ok'], bad)
            self.assertNotIn(str(self.data), json.dumps(res))
            self.assertNotIn(str(target), json.dumps(res))
        self.assertEqual(target.read_text(encoding='utf-8'), 'a = 1\n')

    def test_preview_shows_diff_and_changes_nothing(self):
        target, rid, digest = self._seed('src/c.py', 'v = 1\n', 'v = 2\n', '3' * 32)
        res = self.call('pi_review_preview', self._params(rid, digest))
        self.assertTrue(res['ok'], res)
        r = res['result']
        self.assertEqual(r['schema'], 'navi-review-preview-v1')
        self.assertIs(r['ok'], True)
        self.assertIn('-v = 1', r['diff'])
        self.assertIn('+v = 2', r['diff'])
        self.assertEqual(target.read_text(encoding='utf-8'), 'v = 1\n')
        self.assertNotIn(str(self.data), json.dumps(res))
        self.assertNotIn(str(target), json.dumps(res))

    def test_digest_mismatch_reports_failure(self):
        target, rid, digest = self._seed('src/d.py', 'k = 1\n', 'k = 2\n', '4' * 32)
        wrong = ('0' if digest[0] != '0' else '1') * 64
        res = self.call('pi_review_preview', self._params(rid, wrong))
        self.assertTrue(res['ok'], res)
        self.assertIs(res['result']['ok'], False)
        self.assertEqual(res['result']['reason'], 'digest_mismatch')
        self.assertEqual(target.read_text(encoding='utf-8'), 'k = 1\n')


if __name__ == '__main__':
    unittest.main()
