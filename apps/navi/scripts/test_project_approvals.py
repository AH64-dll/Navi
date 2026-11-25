"""Offline exact-once project read decisions with synthetic owner transport."""
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from project_approvals import ProjectApprovals, ProjectReadError

class ApprovalTests(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)/'project';self.root.mkdir();(self.root/'one.py').write_text('one\ntwo\n');(self.root/'two.py').write_text('other');self.now=0
  self.broker=ProjectApprovals(self.root,'p',clock=lambda:self.now)
 def tearDown(self):self.broker.close();self.temp.cleanup()
 def request(self,path='one.py'):return self.broker.request('read',{'path':path},'p')
 def test_approval_does_not_read_until_exact_decision(self):
  calls=[];original=self.broker.reader.read;self.broker.reader.read=lambda **kw:(calls.append(kw),original(**kw))[1]
  permission=self.request();self.assertEqual(calls,[])
  self.assertEqual(permission['choices'],['allow','deny'])
  self.assertEqual(self.broker.decide(permission['id'],'allow','p')['text'],'one\ntwo\n');self.assertEqual(len(calls),1)
  with self.assertRaises(ProjectReadError):self.broker.decide(permission['id'],'allow','p')
 def test_wrong_id_scope_and_always_cannot_execute(self):
  p=self.request()
  for token,decision,scope in [('bad','allow','p'),(p['id'],'allow','other'),(p['id'],'always','p')]:
   with self.assertRaises(ProjectReadError):self.broker.decide(token,decision,scope)
  self.assertTrue(self.broker.decide(p['id'],'deny','p')['denied'])
 def test_mutating_permission_cannot_change_read(self):
  p=self.request();p['input']['path']='two.py';p['choices'].append('always')
  self.assertEqual(self.broker.decide(p['id'],'allow','p')['text'],'one\ntwo\n')
 def test_expired_decision_is_consumed(self):
  p=self.request();self.now=31
  with self.assertRaises(ProjectReadError):self.broker.decide(p['id'],'allow','p')
  self.assertIsNone(self.broker.pending)
 def test_concurrent_pending_and_cancel(self):
  p=self.request()
  with self.assertRaises(ProjectReadError):self.request('two.py')
  self.broker.close()
  with self.assertRaises(ProjectReadError):self.broker.decide(p['id'],'allow','p')
 def test_root_replacement_invalidates_pending(self):
  p=self.request();moved=self.root.with_name('moved');self.root.rename(moved);self.root.mkdir();(self.root/'one.py').write_text('replacement')
  with self.assertRaises(ProjectReadError):self.broker.decide(p['id'],'allow','p')
 def test_failure_consumes_approval(self):
  p=self.request();(self.root/'one.py').unlink()
  with self.assertRaises(OSError):self.broker.decide(p['id'],'allow','p')
  with self.assertRaises(ProjectReadError):self.broker.decide(p['id'],'allow','p')
 def test_invalid_args_never_create_permission(self):
  for args in [{'path':'../x.py'},{'path':'one.py','start_line':True},{'path':'one.py','extra':'x'},{'path':'.env'}]:
   with self.assertRaises(ProjectReadError):self.broker.request('read',args,'p')
   self.assertIsNone(self.broker.pending)
 def test_attempt_budget_is_bounded(self):
  self.broker.remaining=1;p=self.request();self.broker.decide(p['id'],'deny','p')
  with self.assertRaises(ProjectReadError):self.request()
 def test_list_also_requires_exact_approval(self):
  p=self.broker.request('list',{'path':''},'p');self.assertEqual(p['tool'],'project_list')
  self.assertEqual(len(self.broker.decide(p['id'],'allow','p')['entries']),2)
if __name__=='__main__':unittest.main()
