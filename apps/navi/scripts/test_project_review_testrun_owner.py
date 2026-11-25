"""Actual owner-only private review test run route; synthetic project only."""
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


class ReviewTestrunOwnerTests(test_assistant.AssistantTests):
    def setUp(self):
        self.old_flag = os.environ.get('NAVI_PROJECT_REVIEW_TESTRUN')
        os.environ['NAVI_PROJECT_REVIEW_TESTRUN'] = '1'
        self.oldtemp = tempfile.tempdir
        self.workdir = tempfile.mkdtemp(prefix='navi-trproj-', dir=str(Path.home()))
        os.chmod(self.workdir, 0o700)
        tempfile.tempdir = str(Path.home())
        try:
            super().setUp()
        finally:
            tempfile.tempdir = self.oldtemp
        added = self.call('project_add', {'name': 'trproj', 'path': self.workdir})
        self.assertTrue(added['ok'], added)
        self._project = added['result']['project']
        self.call('assistant_scope', {'project_id': self._project['id']})

    def project(self):
        return self._project

    def tearDown(self):
        try:
            super().tearDown()
        finally:
            import shutil
            shutil.rmtree(self.workdir, ignore_errors=True)
        if self.old_flag is None:
            os.environ.pop('NAVI_PROJECT_REVIEW_TESTRUN', None)
        else:
            os.environ['NAVI_PROJECT_REVIEW_TESTRUN'] = self.old_flag

    def _confirm(self, rid):
        return f'run tests on private copy {rid}'

    def _params(self, review_id, digest, confirm=None):
        return {'project_id': self.project()['id'], 'review_id': review_id, 'proposal_digest': digest,
                'confirm': self._confirm(review_id) if confirm is None else confirm}

    def _seed(self, rel, before, after, request_id):
        root = Path(self.project()['path'])
        (root / 'tests').mkdir(exist_ok=True)
        t = root / 'tests' / 'test_m.py'
        t.write_text('import unittest, sys\nsys.path.insert(0, ".")\nfrom src import m\n'
                     'class T(unittest.TestCase):\n def test_v(self): self.assertEqual(m.V, 1)\n', encoding='utf-8')
        os.chmod(t, 0o600)
        (root / 'src').mkdir(exist_ok=True)
        (root / 'src' / '__init__.py').write_text('', encoding='utf-8')
        target = root / rel
        target.write_text(before, encoding='utf-8')
        os.chmod(target, 0o600)
        sha = hashlib.sha256(before.encode('utf-8')).hexdigest()
        proposal = propose_edit(rel, sha, before, after)
        created = create_review_workspace(owner_review_storage(self.data), proposal, request_id=request_id)
        return target, created['review_id'], proposal_digest(proposal)

    def _leftovers(self):
        return sorted(p for p in os.listdir(tempfile.gettempdir()) if p.startswith('navi-testrun-'))

    def test_default_off(self):
        target, rid, digest = self._seed('src/m.py', 'V = 1\n', 'V = 1\n# c\n', '1' * 32)
        self.process.terminate()
        self.process.wait(3)
        env = dict(self.environment)
        env.pop('NAVI_PROJECT_REVIEW_TESTRUN', None)
        self.process = subprocess.Popen([str(test_assistant.BINARY), 'serve'], env=env,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(0.15)
        self.call('assistant_scope', {'project_id': self.project()['id']})
        res = self.call('pi_review_testrun', self._params(rid, digest))
        self.assertFalse(res['ok'], res)
        self.assertIn('disabled', res['error'].lower())

    def test_strict_params_and_confirm(self):
        target, rid, digest = self._seed('src/m.py', 'V = 1\n', 'V = 2\n', '2' * 32)
        good = self._params(rid, digest)
        for bad in [{}, {k: v for k, v in good.items() if k != 'confirm'}, {**good, 'confirm': 'yes'},
                    {**good, 'confirm': f'apply private review {rid}'}, {**good, 'confirm': ' ' + self._confirm(rid)},
                    {**good, 'proposal_digest': 'A' * 64}, {**good, 'review_id': '../' + rid},
                    {**good, 'path': 'src/m.py'}, {**good, 'argv': ['sh']}]:
            res = self.call('pi_review_testrun', bad)
            self.assertFalse(res['ok'], bad)
            self.assertNotIn(str(self.data), json.dumps(res))
            self.assertNotIn(str(target), json.dumps(res))
        self.assertEqual(target.read_text(encoding='utf-8'), 'V = 1\n')

    def test_run_passes_and_fails_on_private_copy_only(self):
        before_left = self._leftovers()
        target, rid, digest = self._seed('src/m.py', 'V = 1\n', 'V = 1\n# harmless\n', '3' * 32)
        res = self.call('pi_review_testrun', self._params(rid, digest))
        self.assertTrue(res['ok'], res)
        r = res['result']
        self.assertEqual(r['schema'], 'navi-review-testrun-v1')
        if r.get('ok') is False and r.get('reason') == 'sandbox_unavailable':
            self.skipTest('bwrap unavailable on this host')
        self.assertIs(r['ok'], True, r)
        self.assertIs(r['passed'], True, r)
        self.assertEqual(target.read_text(encoding='utf-8'), 'V = 1\n')
        self.assertNotIn(str(self.data), json.dumps(res))
        self.assertNotIn(str(target), json.dumps(res))
        self.assertNotIn(str(self.project()['path']), json.dumps(res))
        target2, rid2, digest2 = self._seed('src/m.py', 'V = 1\n', 'V = 2\n', '4' * 32)
        res = self.call('pi_review_testrun', self._params(rid2, digest2))
        self.assertTrue(res['ok'], res)
        self.assertIs(res['result']['ok'], True, res)
        self.assertIs(res['result']['passed'], False, res)
        self.assertEqual(target2.read_text(encoding='utf-8'), 'V = 1\n')
        self.assertEqual(self._leftovers(), before_left)

    def test_stale_target_refuses_without_running(self):
        target, rid, digest = self._seed('src/m.py', 'V = 1\n', 'V = 2\n', '5' * 32)
        target.write_text('V = 9\n', encoding='utf-8')
        os.chmod(target, 0o600)
        res = self.call('pi_review_testrun', self._params(rid, digest))
        self.assertTrue(res['ok'], res)
        self.assertEqual(res['result'], {'schema': 'navi-review-testrun-v1', 'review_id': rid,
                                         'ok': False, 'reason': 'stale_target'})
        self.assertEqual(target.read_text(encoding='utf-8'), 'V = 9\n')


if __name__ == '__main__':
    own = [n for n in vars(ReviewTestrunOwnerTests) if n.startswith('test_')]
    result = unittest.TextTestRunner(verbosity=1).run(
        unittest.TestSuite(ReviewTestrunOwnerTests(n) for n in own))
    sys.exit(0 if result.wasSuccessful() else 1)
