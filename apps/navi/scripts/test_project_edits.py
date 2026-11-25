"""Pure synthetic edit proposals; no file read/write/apply occurs."""
from pathlib import Path
from dataclasses import FrozenInstanceError
import hashlib
import sys
import unittest
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from project_edits import propose_edit,ProjectReadError

def digest(value):return hashlib.sha256(value.encode()).hexdigest()
class EditTests(unittest.TestCase):
 def propose(self,before='one\n',after='two\n',path='src/code.py',sha=None):return propose_edit(path,digest(before) if sha is None else sha,before,after)
 def test_exact_content_digest_and_diff(self):
  p=self.propose();self.assertEqual(p.before_sha256,digest('one\n'));self.assertEqual(p.after_sha256,digest('two\n'));self.assertIn('-one\n+two\n',p.diff);self.assertFalse(p.review()['applied']);self.assertTrue(p.review()['requires_owner_review'])
 def test_stale_snapshot_refused(self):
  with self.assertRaises(ProjectReadError):self.propose(sha=digest('another\n'))
 def test_digest_must_be_full_canonical_sha(self):
  for sha in ['',None,'a'*63,'G'*64,'A'*64]:
   with self.assertRaises(ProjectReadError):propose_edit('x.py',sha,'one','two')
 def test_paths_cannot_escape_or_target_sensitive_names(self):
  for path in ['../x.py','/x.py','a/../x.py','.env','secret.pem','secrets.json','a\\x.py','a\nx.py']:
   with self.assertRaises(ProjectReadError):self.propose(path=path)
 def test_content_and_line_bounds(self):
  for before,after in [('a'*16385,'b'),('a','b'*16385),('x\n'*1001,'b'),('a','b\n'*1001)]:
   with self.assertRaises(ProjectReadError):self.propose(before,after)
 def test_binary_and_directional_controls_refused(self):
  for control in ['\0','\x1b','\x7f','\u202e','\u2066','\ud800']:
   for before,after in [('a'+control,'b'),('a','b'+control)]:
    with self.assertRaises(ProjectReadError):self.propose(before,after,sha='0'*64)
 def test_noop_refused(self):
  with self.assertRaises(ProjectReadError):self.propose('same','same')
 def test_immutable_binding_and_review_copy(self):
  p=self.propose()
  with self.assertRaises(FrozenInstanceError):p.after='malicious'
  v=p.review();v['after']='changed';self.assertEqual(p.after,'two\n')
 def test_eof_newline_state_is_exact(self):
  p=self.propose('one','one\n');self.assertNotEqual(p.before_sha256,p.after_sha256);self.assertEqual(p.review()['before'],'one');self.assertEqual(p.review()['after'],'one\n')
 def test_unicode_and_crlf_preserved(self):
  p=self.propose('مرحبا\r\n','أهلا\r\n');self.assertEqual(p.after,'أهلا\r\n');self.assertEqual(p.after_sha256,digest('أهلا\r\n'))
 def test_not_an_apply_api(self):
  import project_edits
  self.assertFalse(hasattr(project_edits,'apply'));self.assertFalse(hasattr(project_edits,'open'));self.assertFalse(hasattr(project_edits,'os'))
if __name__=='__main__':unittest.main()
