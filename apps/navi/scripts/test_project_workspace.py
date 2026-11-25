"""Actual private review files, no project edit or execution."""
from pathlib import Path
import hashlib,json,os,stat,sys,tempfile,threading,unittest,subprocess
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from project_edits import propose_edit,ProjectReadError
from project_workspace import create_review_workspace,inspect_review_workspace,workspace_request
class WorkspaceTests(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory(prefix='navi-review-',dir=Path.home());self.root=Path(self.tmp.name);self.p=propose_edit('src/app.py',hashlib.sha256(b'pass\n').hexdigest(),'pass\n','raise RuntimeError("never run")\n')
 def tearDown(self):self.tmp.cleanup()
 def test_exact_private_inert_files(self):
  result=create_review_workspace(self.root,self.p);folder=self.root/result['review_id'];self.assertFalse(result['applied']);self.assertEqual((folder/'before.txt').read_bytes(),b'pass\n');self.assertEqual((folder/'proposed-after.txt').read_bytes(),self.p.after.encode());self.assertEqual(stat.S_IMODE(folder.stat().st_mode),0o700)
  for path in folder.iterdir():self.assertEqual(stat.S_IMODE(path.stat().st_mode),0o600)
  manifest=json.loads((folder/'review.json').read_text());self.assertEqual(manifest['original_path'],'src/app.py');self.assertTrue(manifest['requires_fresh_validation']);self.assertFalse((self.root/'src').exists())
 def test_repeat_creates_new_workspace_never_overwrites(self):
  first=create_review_workspace(self.root,self.p);second=create_review_workspace(self.root,self.p);self.assertNotEqual(first['review_id'],second['review_id'])
 def test_path_is_metadata_not_destination(self):
  result=create_review_workspace(self.root,self.p);self.assertEqual(set((self.root/result['review_id']).iterdir()),{self.root/result['review_id']/n for n in ['before.txt','proposed-after.txt','review.json']})
 def test_symlink_storage_refused(self):
  link=self.root/'link';link.symlink_to(self.root,target_is_directory=True)
  with self.assertRaises(OSError):create_review_workspace(link,self.p)
 def test_nonprivate_storage_refused(self):
  self.root.chmod(0o755)
  with self.assertRaises(ProjectReadError):create_review_workspace(self.root,self.p)
 def test_worldwritable_ancestor_refused(self):
  with tempfile.TemporaryDirectory() as folder:
   with self.assertRaises(ProjectReadError):create_review_workspace(folder,self.p)
 def test_precancel_creates_nothing(self):
  cancel=threading.Event();cancel.set()
  with self.assertRaises(ProjectReadError):create_review_workspace(self.root,self.p,cancel=cancel)
  self.assertEqual(list(self.root.iterdir()),[])
 def test_partial_write_failure_cleans_new_files(self):
  with patch('project_workspace.os.write',side_effect=OSError('synthetic failure')):
   with self.assertRaises(OSError):create_review_workspace(self.root,self.p)
  self.assertEqual(list(self.root.iterdir()),[])
 def test_cancel_during_write_cleans_new_files(self):
  cancel=threading.Event();real=os.write
  def write(fd,data):result=real(fd,data);cancel.set();return result
  with patch('project_workspace.os.write',side_effect=write):
   with self.assertRaises(ProjectReadError):create_review_workspace(self.root,self.p,cancel=cancel)
  self.assertEqual(list(self.root.iterdir()),[])
 def created(self):
  result=create_review_workspace(self.root,self.p);return result['review_id'],self.root/result['review_id']
 def test_recovery_verifies_exact_hashes(self):
  name,folder=self.created();result=inspect_review_workspace(self.root,name);self.assertTrue(result['valid']);self.assertEqual(result['proposal']['after'],self.p.after);self.assertFalse(result['applied'])
 def test_tampered_snapshot_refused(self):
  name,folder=self.created();(folder/'proposed-after.txt').write_text('modified')
  with self.assertRaises(ProjectReadError):inspect_review_workspace(self.root,name)
 def test_incomplete_or_extra_files_refused(self):
  for extra in [True,False]:
   name,folder=self.created()
   if extra:(folder/'unexpected').write_text('')
   else:(folder/'review.json').unlink()
   with self.assertRaises(ProjectReadError):inspect_review_workspace(self.root,name)
 def test_review_identifier_cannot_be_path(self):
  for name in ['../outside','review-'+'A'*32,'review-'+'0'*31,'/tmp']:
   with self.assertRaises(ProjectReadError):inspect_review_workspace(self.root,name)
 def test_symlink_snapshot_and_hardlink_refused(self):
  for hard in [True,False]:
   name,folder=self.created();original=folder/'before.txt';original.unlink();outside=self.root/('outside'+str(hard));outside.write_text('pass\n');outside.chmod(0o600)
   if hard:os.link(outside,original)
   else:original.symlink_to(outside)
   with self.assertRaises((ProjectReadError,OSError)):inspect_review_workspace(self.root,name)
 def test_fifo_snapshot_refused_without_blocking(self):
  name,folder=self.created();path=folder/'before.txt';path.unlink();os.mkfifo(path,0o600)
  with self.assertRaises(ProjectReadError):inspect_review_workspace(self.root,name)
 def test_duplicate_manifest_key_and_oversize_refused(self):
  for text in ['{"schema":1,"schema":2}', 'x'*16385]:
   name,folder=self.created();(folder/'review.json').write_text(text)
   with self.assertRaises(ProjectReadError):inspect_review_workspace(self.root,name)
 def test_malformed_after_digest_and_manifest_fields_refused(self):
  name,folder=self.created();path=folder/'review.json';value=json.loads(path.read_text());value['after_sha256']='0'*64;path.write_text(json.dumps(value))
  with self.assertRaises(ProjectReadError):inspect_review_workspace(self.root,name)
 def test_idempotent_owner_request_recovers_without_duplicate(self):
  key='a'*32;first=create_review_workspace(self.root,self.p,request_id=key);second=create_review_workspace(self.root,self.p,request_id=key)
  self.assertEqual(first['review_id'],second['review_id']);self.assertTrue(second['recovered']);self.assertFalse(second['created']);self.assertEqual(len(list(self.root.iterdir())),1)
 def test_same_request_cannot_change_proposal(self):
  create_review_workspace(self.root,self.p,request_id='a'*32);changed=propose_edit('src/app.py',self.p.before_sha256,self.p.before,'print("different")\n')
  with self.assertRaises(ProjectReadError):create_review_workspace(self.root,changed,request_id='a'*32)
  self.assertEqual(len(list(self.root.iterdir())),1)
 def test_partial_same_request_blocks_without_new_directory(self):
  result=create_review_workspace(self.root,self.p,request_id='a'*32);folder=self.root/result['review_id'];(folder/'review.json').unlink()
  with self.assertRaises(ProjectReadError):create_review_workspace(self.root,self.p,request_id='a'*32)
  self.assertEqual(len(list(self.root.iterdir())),1)
 def test_idempotency_key_cannot_choose_path(self):
  for key in ['../outside','0'*31,'G'*32,False]:
   with self.assertRaises(ProjectReadError):create_review_workspace(self.root,self.p,request_id=key)
  self.assertEqual(list(self.root.iterdir()),[])
 def frame(self,key='b'*32):
  p=self.p
  return (json.dumps({'request_id':key,'preview':{'schema':'navi-edit-preview-v1','applied':False,'requires_fresh_validation':True,'path':p.path,'before_sha256':p.before_sha256,'after_sha256':p.after_sha256,'before':p.before,'after':p.after}})+'\n').encode()
 def test_fixed_cli_create_and_recover(self):
  worker=Path(__file__).resolve().parents[1]/'workers/project_workspace.py'
  results=[]
  for _ in range(2):
   result=subprocess.run([sys.executable,'-I',str(worker),str(self.root)],input=self.frame(),capture_output=True,timeout=3);self.assertEqual(result.returncode,0,result.stderr);results.append(json.loads(result.stdout))
  self.assertEqual(results[0]['review_id'],results[1]['review_id']);self.assertTrue(results[1]['recovered']);self.assertEqual(len(list(self.root.iterdir())),1)
 def test_transport_has_no_storage_root_or_command(self):
  for field,value in [('root','/'),('storage','/'),('command','run'),('decision','allow')]:
   request=json.loads(self.frame());request[field]=value
   with self.assertRaises(ProjectReadError):workspace_request(self.root,(json.dumps(request)+'\n').encode())
  self.assertEqual(list(self.root.iterdir()),[])
 def test_duplicate_transport_keys_refused(self):
  with self.assertRaises(ProjectReadError):workspace_request(self.root,b'{"request_id":1,"request_id":2}\n')
if __name__=='__main__':unittest.main()
