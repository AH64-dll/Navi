"""Actual owner-only private review creation/recovery; synthetic project only."""
import os,sys,json,hashlib,subprocess,tempfile,time,unittest,threading,socket,shutil
from pathlib import Path
from unittest.mock import patch
import test_assistant
class WorkspaceOwnerTests(test_assistant.AssistantTests):
 def setUp(self):
  self.old=os.environ.get('NAVI_PROJECT_WORKSPACES');os.environ['NAVI_PROJECT_WORKSPACES']='1'
  self.oldtemp=tempfile.tempdir;tempfile.tempdir=str(Path.home())
  try:super().setUp()
  finally:tempfile.tempdir=self.oldtemp
  self.call('assistant_scope',{'project_id':self.project()['id']})
 def tearDown(self):
  super().tearDown()
  if self.old is None:os.environ.pop('NAVI_PROJECT_WORKSPACES',None)
  else:os.environ['NAVI_PROJECT_WORKSPACES']=self.old
 def params(self):
  before='pass\n';after='raise RuntimeError("never")\n';sha=lambda x:hashlib.sha256(x.encode()).hexdigest()
  return {'project_id':self.project()['id'],'request_id':'a'*32,'preview':{'schema':'navi-edit-preview-v1','applied':False,'requires_fresh_validation':True,'path':'synthetic.py','before_sha256':sha(before),'after_sha256':sha(after),'before':before,'after':after}}
 def test_create_and_same_id_recover(self):
  p=self.params();first=self.call('pi_preview_workspace',p);self.assertTrue(first['ok'],first);second=self.call('pi_preview_workspace',p);self.assertTrue(second['ok'],second);self.assertTrue(second['result']['recovered']);self.assertEqual(first['result']['review_id'],second['result']['review_id']);folder=Path(first['result']['storage_path']);self.assertEqual((folder/'proposed-after.txt').read_text(),p['preview']['after']);self.assertFalse((Path(self.project()['path'])/'synthetic.py').exists());self.assertEqual(self.posts('/sessions'),[])
 def test_same_id_changed_proposal_refused(self):
  p=self.params();self.assertTrue(self.call('pi_preview_workspace',p)['ok']);p['preview']['after']='print("changed")\n';p['preview']['after_sha256']=hashlib.sha256(p['preview']['after'].encode()).hexdigest();self.assertFalse(self.call('pi_preview_workspace',p)['ok']);self.assertEqual(len(list((self.data/'proposal-reviews').iterdir())),1)
 def test_no_roots_commands_or_extra_fields(self):
  for field,value in [('root','/'),('storage','/'),('command','run')]:
   p=self.params();p[field]=value;self.assertFalse(self.call('pi_preview_workspace',p)['ok'])
 def test_partial_recovery_is_blocked(self):
  p=self.params();first=self.call('pi_preview_workspace',p);self.assertTrue(first['ok'],first);(Path(first['result']['storage_path'])/'review.json').unlink();again=self.call('pi_preview_workspace',p);self.assertFalse(again['ok']);self.assertIn('same request ID',again['error']);self.assertEqual(len(list((self.data/'proposal-reviews').iterdir())),1)
 def test_default_off(self):
  self.process.terminate();self.process.wait(3);env=dict(self.environment);env.pop('NAVI_PROJECT_WORKSPACES',None);self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE);time.sleep(.15);result=self.call('pi_preview_workspace',self.params());self.assertFalse(result['ok']);self.assertIn('disabled',result['error'])
 def delayed_reply(self):
  # The real helper creates complete inert files before a fixture delays reply.
  self.process.terminate();self.process.wait(3);app=self.folder/'fixed-app';shutil.copytree(test_assistant.ROOT,app,ignore=shutil.ignore_patterns('target','__pycache__'))
  marker=self.folder/'workspace-created';worker=app/'workers/project_workspace.py';text=worker.read_text();text=text.replace('print(json.dumps(result),flush=True)','Path('+repr(str(marker))+').write_text(str(os.getpid()))\n        import time; time.sleep(2)\n        print(json.dumps(result),flush=True)',1);worker.write_text(text)
  env={**self.environment,'NAVI_APP_DIR':str(app)};self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE);time.sleep(.15);self.call('assistant_scope',{'project_id':self.project()['id']})
  return marker
 def wait_created(self,marker):
  for _ in range(100):
   if marker.exists():return int(marker.read_text())
   time.sleep(.02)
  self.fail('workspace never created')
 def restart_real(self,project_id):
  self.process.terminate();self.process.wait(3);self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=self.environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE);time.sleep(.15);self.call('assistant_scope',{'project_id':project_id})
 def assert_same_recovery(self,p,pid):
  for _ in range(100):
   try:os.kill(pid,0)
   except ProcessLookupError:break
   time.sleep(.02)
  else:self.fail('workspace helper not reaped')
  self.restart_real(p['project_id']);result=self.call('pi_preview_workspace',p);self.assertTrue(result['ok'],result);self.assertTrue(result['result']['recovered']);self.assertEqual(len(list((self.data/'proposal-reviews').iterdir())),1)
 def cancel_created(self,mode):
  marker=self.delayed_reply();p=self.params();result={};task=threading.Thread(target=lambda:result.update(value=self.call('pi_preview_workspace',p)));task.start();pid=self.wait_created(marker)
  if mode=='stop':self.call('cancel')
  else:
   other=self.folder/'other';other.mkdir();row=self.call('project_add',{'name':'Other','path':str(other)});self.call('assistant_scope',{'project_id':row['result']['project']['id']})
  task.join(3);self.assertFalse(task.is_alive());self.assertFalse(result['value']['ok'],result);self.assertIn('same request ID',result['value']['error']);self.assert_same_recovery(p,pid)
 def test_created_before_stop_recovers_without_duplicate(self):self.cancel_created('stop')
 def test_created_before_project_switch_recovers_without_duplicate(self):self.cancel_created('switch')
 def test_created_before_owner_eof_recovers_without_duplicate(self):
  marker=self.delayed_reply();p=self.params();connection=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);connection.connect(str(self.runtime/'station.sock'));connection.sendall((json.dumps({'op':'pi_preview_workspace','params':p})+'\n').encode());pid=self.wait_created(marker);connection.close();self.assert_same_recovery(p,pid)
if __name__=='__main__':
 suite=unittest.TestSuite(WorkspaceOwnerTests(n) for n in WorkspaceOwnerTests.__dict__ if n.startswith('test_'));raise SystemExit(not unittest.TextTestRunner().run(suite).wasSuccessful())
