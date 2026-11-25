"""Synthetic review state only, no filesystem mutation."""
from pathlib import Path
from dataclasses import replace
import hashlib
import sys
import unittest
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from project_edit_reviews import EditReviews,proposal_digest,ProjectReadError
from project_edits import propose_edit
class ReviewTests(unittest.TestCase):
 def setUp(self):
  self.sha=hashlib.sha256(b'one\n').hexdigest();self.proposal=propose_edit('code.py',self.sha,'one\n','two\n');self.now=0;self.broker=EditReviews('turn',clock=lambda:self.now)
 def tearDown(self):self.broker.close()
 def request(self):return self.broker.request(self.proposal,'turn')
 def decide(self,p,decision='accept_proposal',**changes):
  v={'token':p['id'],'digest':p['proposal_digest'],'decision':decision,'scope':'turn','current_snapshot_sha256':self.sha};v.update(changes);return self.broker.decide(**v)
 def test_exact_review_does_not_apply(self):
  p=self.request();self.assertEqual(p['choices'],['accept_proposal','reject']);r=self.decide(p);self.assertTrue(r['accepted']);self.assertFalse(r['applied']);self.assertFalse(r['proposal']['applied'])
 def test_reject(self):self.assertEqual(self.decide(self.request(),'reject'),{'accepted':False,'applied':False})
 def test_wrong_scope_id_digest_or_apply_never_accepts(self):
  p=self.request()
  for changes in [{'token':'wrong'},{'scope':'other'},{'digest':'0'*64},{'decision':'allow'},{'decision':'apply'},{'decision':'always'}]:
   with self.assertRaises(ProjectReadError):self.decide(p,**changes)
  self.assertFalse(self.decide(p,'reject')['accepted'])
 def test_replay_refused(self):
  p=self.request();self.decide(p)
  with self.assertRaises(ProjectReadError):self.decide(p)
 def test_stale_snapshot_consumes_review(self):
  p=self.request()
  with self.assertRaises(ProjectReadError):self.decide(p,current_snapshot_sha256='0'*64)
  with self.assertRaises(ProjectReadError):self.decide(p)
 def test_timeout_consumes_review(self):
  p=self.request();self.now=60
  with self.assertRaises(ProjectReadError):self.decide(p)
  self.assertIsNone(self.broker.pending)
 def test_cancel_and_new_scope(self):
  p=self.request();self.broker.close()
  with self.assertRaises(ProjectReadError):self.decide(p)
  with self.assertRaises(ProjectReadError):self.request()
 def test_presented_copy_cannot_change_proposal(self):
  p=self.request();p['proposal']['after']='changed';r=self.decide(p);self.assertEqual(r['proposal']['after'],'two\n')
 def test_forged_proposal_binding_refused(self):
  for p in [replace(self.proposal,after_sha256='0'*64),replace(self.proposal,diff='false diff'),replace(self.proposal,path='../code.py')]:
   with self.assertRaises(ProjectReadError):proposal_digest(p)
 def test_pending_and_total_budget_bounded(self):
  p=self.request()
  with self.assertRaises(ProjectReadError):self.request()
  self.decide(p,'reject')
  for _ in range(3):self.decide(self.request(),'reject')
  with self.assertRaises(ProjectReadError):self.request()
 def test_digest_binds_path_before_after(self):
  self.assertNotEqual(proposal_digest(self.proposal),proposal_digest(replace(self.proposal,path='other.py',diff=self.proposal.diff.replace('code.py','other.py'))))
if __name__=='__main__':unittest.main()
