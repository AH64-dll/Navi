"""Synthetic project fixtures only. No model or private project access."""
import os
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from project_read import ProjectReader, ProjectReadError

class ProjectTests(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);self.project=self.root/'project';self.project.mkdir();self.reader=ProjectReader(self.project)
 def tearDown(self):
  self.reader.close();self.temp.cleanup()
 def test_read_lines_and_provenance(self):
  (self.project/'code.py').write_text('one\ntwo\nthree\n');r=self.reader.read('code.py',2,1)
  self.assertEqual(r['text'],'two\n');self.assertTrue(r['truncated']);self.assertEqual(r['source'],'untrusted-project-data')
 def test_traversal_and_sensitive_names_refused(self):
  for path in ['../outside.txt','/etc/passwd','a/../b.py','./x.py','x//y.py','.env','a/.git/config','secrets.json','tokens.txt','a\\b.py','x\x00.py']:
   with self.assertRaises((ProjectReadError,OSError)):self.reader.read(path)
 def test_symlink_file_and_directory_refused(self):
  outside=self.root/'outside.txt';outside.write_text('private');(self.project/'link.txt').symlink_to(outside);(self.project/'dir').symlink_to(self.root,target_is_directory=True)
  for p in ['link.txt','dir/outside.txt']:
   with self.assertRaises(OSError):self.reader.read(p)
 def test_hardlink_refused(self):
  outside=self.root/'outside.txt';outside.write_text('private');os.link(outside,self.project/'hard.txt')
  with self.assertRaises(ProjectReadError):self.reader.read('hard.txt')
 def test_fifo_refused_without_blocking(self):
  os.mkfifo(self.project/'fifo.txt')
  with self.assertRaises(ProjectReadError):self.reader.read('fifo.txt')
 def test_binary_utf8_and_extension_policy(self):
  for name,data in [('binary.txt',b'a\0b'),('bad.txt',b'\xff'),('key.pem',b'private')]:
   (self.project/name).write_bytes(data)
   with self.assertRaises(ProjectReadError):self.reader.read(name)
 def test_input_and_output_bounds(self):
  (self.project/'big.txt').write_bytes(b'x'*65537)
  with self.assertRaises(ProjectReadError):self.reader.read('big.txt')
  (self.project/'text.txt').write_text('z'*20000);r=self.reader.read('text.txt');self.assertEqual(len(r['text']),16384);self.assertTrue(r['truncated'])
 def test_list_bounded_excludes_links_hidden_and_sensitive(self):
  for i in range(205):(self.project/f'f{i}.txt').touch()
  (self.project/'.env').touch();(self.project/'secrets.json').touch();(self.project/'link').symlink_to(self.root)
  r=self.reader.list();self.assertEqual(len(r['entries']),200);self.assertTrue(r['truncated']);self.assertFalse(any(e['name'].startswith('.') or e['name']=='link' for e in r['entries']))
 def test_pinned_directory_survives_path_replacement_without_escape(self):
  (self.project/'old.py').write_text('original');moved=self.root/'moved';self.project.rename(moved);self.project.symlink_to(self.root,target_is_directory=True)
  self.assertEqual(self.reader.read('old.py')['text'],'original')
 def test_root_symlink_relative_and_closed_refused(self):
  link=self.root/'alias';link.symlink_to(self.project,target_is_directory=True)
  with self.assertRaises((ProjectReadError,OSError)):ProjectReader(link)
  with self.assertRaises(ProjectReadError):ProjectReader('relative')
  self.reader.close()
  with self.assertRaises(ProjectReadError):self.reader.list()
 def test_invalid_line_bounds(self):
  for start,count in [(0,1),(1,201),(True,1),(1,False)]:
   with self.assertRaises(ProjectReadError):self.reader.read('code.py',start,count)
 def test_snapshot_digest_only_for_complete_approved_read(self):
  import hashlib
  raw='one\ntwo\n';(self.project/'small.py').write_text(raw);result=self.reader.read('small.py');self.assertEqual(result['snapshot_sha256'],hashlib.sha256(raw.encode()).hexdigest());self.assertEqual(result['snapshot_bytes'],len(raw))
  for start,count in [(2,200),(1,1)]:self.assertNotIn('snapshot_sha256',self.reader.read('small.py',start,count))
  (self.project/'large.py').write_text('x'*20000);self.assertNotIn('snapshot_sha256',self.reader.read('large.py'))
 def test_empty_file_snapshot_is_complete(self):
  import hashlib
  (self.project/'empty.py').write_text('');result=self.reader.read('empty.py');self.assertEqual(result['snapshot_sha256'],hashlib.sha256(b'').hexdigest());self.assertEqual(result['snapshot_bytes'],0)
if __name__=='__main__':unittest.main()
