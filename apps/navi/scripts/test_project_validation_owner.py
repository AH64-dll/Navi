"""Actual owner-only isolated syntax route with synthetic previews."""
import os,json,hashlib,subprocess,time,unittest,threading,socket,shutil
from pathlib import Path
import test_assistant
class ValidationOwnerTests(test_assistant.AssistantTests):
 def setUp(self):
  self.old=os.environ.get('NAVI_PROJECT_VALIDATE');os.environ['NAVI_PROJECT_VALIDATE']='1';super().setUp();self.call('assistant_scope',{'project_id':self.project()['id']})
 def tearDown(self):
  super().tearDown()
  if self.old is None:os.environ.pop('NAVI_PROJECT_VALIDATE',None)
  else:os.environ['NAVI_PROJECT_VALIDATE']=self.old
 def params(self,after='raise RuntimeError("never execute")\n'):
  before='pass\n';sha=lambda x:hashlib.sha256(x.encode()).hexdigest()
  return {'project_id':self.project()['id'],'preview':{'schema':'navi-edit-preview-v1','applied':False,'requires_fresh_validation':True,'path':'synthetic.py','before_sha256':sha(before),'after_sha256':sha(after),'before':before,'after':after}}
 def test_actual_fixed_parser_never_executes(self):
  result=self.call('pi_preview_validate',self.params());self.assertTrue(result['ok'],result);self.assertTrue(result['result']['result']['valid']);self.assertFalse(result['result']['executed_project_code']);self.assertFalse(result['result']['applied']);self.assertEqual(self.posts('/sessions'),[])
 def test_syntax_error(self):
  result=self.call('pi_preview_validate',self.params('def broken(\n'));self.assertTrue(result['ok'],result);self.assertFalse(result['result']['result']['valid'])
 def test_no_root_command_or_changed_digest(self):
  for field,value in [('root','/'),('command','run'),('after','modified')]:
   p=self.params();p['preview'][field]=value;self.assertFalse(self.call('pi_preview_validate',p)['ok'])
 def test_explicit_current_project_required(self):
  p=self.params();p.pop('project_id');self.assertFalse(self.call('pi_preview_validate',p)['ok'])
  p=self.params();p['project_id']='nonexistent';self.assertFalse(self.call('pi_preview_validate',p)['ok'])
 def test_default_off(self):
  self.process.terminate();self.process.wait(3);env=dict(self.environment);env.pop('NAVI_PROJECT_VALIDATE',None);self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE);time.sleep(.15)
  result=self.call('pi_preview_validate',self.params());self.assertFalse(result['ok']);self.assertIn('disabled',result['error'])
 def test_json_owner_syntax_only(self):
  p=self.params('{"only":"data"}');p['preview']['path']='synthetic.json'
  result=self.call('pi_preview_validate',p);self.assertTrue(result['ok'],result);self.assertTrue(result['result']['result']['valid']);self.assertEqual(result['result']['kind'],'json-syntax-only')
 def test_json_duplicate_keys_refused(self):
  p=self.params('{"key":1,"key":2}');p['preview']['path']='synthetic.json'
  result=self.call('pi_preview_validate',p);self.assertTrue(result['ok'],result);self.assertFalse(result['result']['result']['valid'])
 def test_javascript_owner_fixed_parser(self):
  p=self.params('import missing from "not-installed";throw Error("never");');p['preview']['path']='synthetic.mjs'
  result=self.call('pi_preview_validate',p);self.assertTrue(result['ok'],result);self.assertTrue(result['result']['result']['valid']);self.assertEqual(result['result']['kind'],'javascript-syntax-only')
 def delayed(self):
  # Temporary installed-app fixture, never a project-selected executable.
  self.process.terminate();self.process.wait(3)
  app=self.folder/'fixed-app';shutil.copytree(test_assistant.ROOT,app,ignore=shutil.ignore_patterns('target','__pycache__'))
  marker=self.folder/'validator-started';finished=self.folder/'validator-finished'
  worker=app/'workers/project_validate.py'
  worker.write_text('import sys,time,os,json\nfrom pathlib import Path\nv=json.loads(sys.stdin.readline())\nPath('+repr(str(marker))+').write_text(str(os.getpid()))\ntime.sleep(2)\nPath('+repr(str(finished))+').write_text("should not finish")\nprint(json.dumps({"kind":"python-syntax-only","path":v["path"],"after_sha256":v["after_sha256"],"applied":False,"executed_project_code":False,"result":{"valid":True}}),flush=True)\n')
  env={**self.environment,'NAVI_APP_DIR':str(app)};self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE);time.sleep(.15);self.call('assistant_scope',{'project_id':self.project()['id']})
  return marker,finished
 def wait_marker(self,marker):
  for _ in range(100):
   if marker.exists():return int(marker.read_text())
   time.sleep(.02)
  self.fail('validator never started')
 def assert_reaped(self,pid,finished):
  for _ in range(100):
   try:os.kill(pid,0)
   except ProcessLookupError:break
   time.sleep(.02)
  else:self.fail('validator process survived cancellation')
  self.assertFalse(finished.exists())
 def run_cancel(self,mode):
  marker,finished=self.delayed();p=self.params();result={}
  task=threading.Thread(target=lambda:result.update(value=self.call('pi_preview_validate',p)));task.start();pid=self.wait_marker(marker)
  if mode=='stop':self.call('cancel')
  else:
   other=self.folder/'other';other.mkdir();row=self.call('project_add',{'name':'Other','path':str(other)});self.call('assistant_scope',{'project_id':row['result']['project']['id']})
  task.join(3);self.assertFalse(task.is_alive());self.assertFalse(result['value']['ok'],result);self.assert_reaped(pid,finished)
 def test_inflight_stop(self):self.run_cancel('stop')
 def test_inflight_project_switch(self):self.run_cancel('switch')
 def test_owner_socket_eof_reaps(self):
  marker,finished=self.delayed();connection=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);connection.connect(str(self.runtime/'station.sock'));connection.sendall((json.dumps({'op':'pi_preview_validate','params':self.params()})+'\n').encode());pid=self.wait_marker(marker);connection.close();self.assert_reaped(pid,finished)
 def test_single_active_validation(self):
  marker,finished=self.delayed();p=self.params();result={};task=threading.Thread(target=lambda:result.update(value=self.call('pi_preview_validate',p)));task.start();pid=self.wait_marker(marker)
  second=self.call('pi_preview_validate',p);self.assertFalse(second['ok']);self.assertIn('already active',second['error']);self.call('cancel');task.join(3);self.assertFalse(result['value']['ok']);self.assert_reaped(pid,finished)
if __name__=='__main__':
 suite=unittest.TestSuite(ValidationOwnerTests(n) for n in ValidationOwnerTests.__dict__ if n.startswith('test_'));raise SystemExit(not unittest.TextTestRunner().run(suite).wasSuccessful())
