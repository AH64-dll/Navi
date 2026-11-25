"""Real isolated daemon owner route, never a provider call."""
import os
import threading
import json
import socket
import time
import unittest
from pathlib import Path
import test_assistant

class OwnerProjectTests(test_assistant.AssistantTests):
 def setUp(self):
  self.old=os.environ.get('NAVI_PI_PROJECT_READ');os.environ['NAVI_PI_PROJECT_READ']='1';super().setUp();self.project();self.root=Path(self.project()['path']);self.file=self.root/'navi-synthetic-owner-fixture.py';self.file.write_text('synthetic owner route\n')
 def tearDown(self):
  self.file.unlink(missing_ok=True);super().tearDown()
  if self.old is None:os.environ.pop('NAVI_PI_PROJECT_READ',None)
  else:os.environ['NAVI_PI_PROJECT_READ']=self.old
 def begin(self):
  result={};t=threading.Thread(target=lambda:result.update(value=self.call('pi_project_read',{'project_id':self.project()['id'],'operation':'read','args':{'path':self.file.name}},timeout=8)));t.start()
  for _ in range(100):
   events=self.call('status')['result']['events'];matches=[e for e in events if e['kind']=='approval' and e['details'].get('id','').startswith('project-perm-')]
   if matches:return t,result,matches[-1]['details']['id']
   time.sleep(.02)
  self.fail('missing project approval')
 def test_owner_route_requires_exact_approval(self):
  t,result,token=self.begin();self.assertTrue(t.is_alive());self.assertFalse(self.call('permission_reply',{'id':token,'decision':'always'})['ok']);self.assertTrue(self.call('permission_reply',{'id':token,'decision':'allow'})['ok']);t.join(3);self.assertFalse(t.is_alive());self.assertEqual(result['value']['result']['text'],'synthetic owner route\n');self.assertFalse(self.call('permission_reply',{'id':token,'decision':'allow'})['ok']);self.assertFalse(self.posts('/permissions/'+token+'/reply'))
 def test_owner_deny(self):
  t,result,token=self.begin();self.assertTrue(self.call('permission_reply',{'id':token,'decision':'deny'})['ok']);t.join(3);self.assertTrue(result['value']['result']['denied'])
 def test_owner_cancel_rejects_late_decision(self):
  t,result,token=self.begin();self.call('cancel');t.join(3);self.assertFalse(t.is_alive());self.assertFalse(result['value']['ok']);self.assertFalse(self.call('permission_reply',{'id':token,'decision':'allow'})['ok'])
 def test_model_root_argument_is_refused(self):
  result=self.call('pi_project_read',{'root':'/','operation':'read','args':{'path':'etc/passwd'}});self.assertFalse(result['ok'])
 def test_disconnect_invalidates_pending_approval(self):
  connection=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);connection.connect(str(self.runtime/'station.sock'));connection.sendall(json.dumps({'op':'pi_project_read','params':{'project_id':self.project()['id'],'operation':'read','args':{'path':self.file.name}}}).encode()+b'\n')
  token=None
  for _ in range(100):
   events=self.call('status')['result']['events'];matches=[e for e in events if e['kind']=='approval' and e['details'].get('id','').startswith('project-perm-')]
   if matches:token=matches[-1]['details']['id'];break
   time.sleep(.02)
  self.assertIsNotNone(token);connection.close();time.sleep(.15);self.assertFalse(self.call('permission_reply',{'id':token,'decision':'allow'})['ok'])
 def test_project_switch_invalidates_pending(self):
  t,result,token=self.begin();other=self.folder/'second-project';other.mkdir();added=self.call('project_add',{'name':'Second','path':str(other)});self.assertTrue(added['ok'],added);self.assertTrue(self.call('assistant_scope',{'project_id':added['result']['project']['id']})['ok']);t.join(3);self.assertFalse(t.is_alive());self.assertFalse(result['value']['ok']);self.assertFalse(self.call('permission_reply',{'id':token,'decision':'allow'})['ok'])
 def test_feature_disabled_without_optin(self):
  self.process.terminate();self.process.wait(3);env=dict(self.environment);env.pop('NAVI_PI_PROJECT_READ',None);self.process=__import__('subprocess').Popen([str(test_assistant.BINARY),'serve'],env=env,stdout=__import__('subprocess').PIPE,stderr=__import__('subprocess').PIPE)
  time.sleep(.15);result=self.call('pi_project_read',{'operation':'list','args':{'path':''}});self.assertFalse(result['ok']);self.assertIn('disabled',result['error'])
 def test_offline_pi_loop_actual_owner_bridge(self):
  import subprocess
  repository=Path(__file__).resolve().parents[3];client=repository/'packages/navi/test/project-offline-client.ts';bundle=self.folder/'offline-project.mjs'
  built=subprocess.run(['node','--input-type=module','-e',"import {build} from 'esbuild';await build({entryPoints:[process.argv[1]],outfile:process.argv[2],bundle:true,platform:'node',format:'esm',packages:'external',alias:{'@earendil-works/pi-ai':process.argv[3], '@earendil-works/pi-telemetry':process.argv[4]}})",str(client),str(bundle),str(repository/'packages/ai/src/index.ts'),str(repository/'packages/telemetry/src/index.ts')],cwd=repository,capture_output=True,text=True);self.assertEqual(built.returncode,0,built.stderr)
  # Keep the bundle under the repository so external packages resolve normally.
  target=repository/'packages/navi/test/.offline-project-fixture.mjs';target.write_bytes(bundle.read_bytes())
  try:
   proc=subprocess.Popen(['node',str(target),str(self.runtime),self.project()['id']],cwd=repository,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
   token=None
   for _ in range(200):
    events=self.call('status')['result']['events'];matches=[e for e in events if e['kind']=='approval' and e['details'].get('id','').startswith('project-perm-')]
    if matches:token=matches[-1]['details']['id'];break
    if proc.poll() is not None:break
    time.sleep(.02)
   self.assertIsNotNone(token,proc.communicate(timeout=2) if proc.poll() is not None else 'no approval')
   self.assertTrue(self.call('permission_reply',{'id':token,'decision':'allow'})['ok']);out,error=proc.communicate(timeout=5);self.assertEqual(proc.returncode,0,error);result=json.loads(out);self.assertEqual(result['reason'],'completed');self.assertIn('Synthetic file verified',out);self.assertNotIn('project-perm-',out);self.assertNotIn(str(self.runtime),out)
  finally:
   target.unlink(missing_ok=True)
   if 'proc' in locals() and proc.poll() is None:proc.kill();proc.communicate()
 def production_harness(self,mode="allow",prompt="delete all files in the project",preview=False):
  import subprocess
  from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
  calls=[]
  class LocalProvider(BaseHTTPRequestHandler):
   def log_message(self,*unused):pass
   def do_POST(handler):
    body=json.loads(handler.rfile.read(int(handler.headers['Content-Length'])));calls.append(body);first=len(calls)==1
    delta={'role':'assistant','tool_calls':[{'index':0,'id':'synthetic','type':'function','function':{'name':'project_read','arguments':json.dumps({'path':self.file.name})}}]} if first else {'role':'assistant','content':'Synthetic local project route completed'}
    if preview and len(calls)==2:
     import hashlib
     delta={'role':'assistant','tool_calls':[{'index':0,'id':'preview','type':'function','function':{'name':'project_propose_edit','arguments':json.dumps({'path':self.file.name,'before_sha256':hashlib.sha256(b'synthetic owner route\n').hexdigest(),'replacement':'suggested replacement\n'})}}]};first=True
    handler.send_response(200);handler.send_header('Content-Type','text/event-stream');handler.end_headers()
    for part,reason in [(delta,None),({},'tool_calls' if first else 'stop')]:handler.wfile.write(('data: '+json.dumps({'id':'local','object':'chat.completion.chunk','created':1,'model':'synthetic','choices':[{'index':0,'delta':part,'finish_reason':reason}]})+'\n\n').encode())
    handler.wfile.write(b'data: [DONE]\n\n')
  provider=ThreadingHTTPServer(('127.0.0.1',0),LocalProvider);threading.Thread(target=provider.serve_forever,daemon=True).start()
  try:
   self.process.terminate();self.process.wait(3);env={**self.environment,'NAVI_PROJECT_ENDPOINT':'http://127.0.0.1:'+str(provider.server_port)+'/v1','NAVI_PROJECT_MODEL':'synthetic','NAVI_PROJECT_PREVIEWS':'1' if preview else '0'};self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE);time.sleep(.15)
   result={};t=threading.Thread(target=lambda:result.update(value=self.call('command',{'text':prompt,'harness':'pi-project','project_id':self.project()['id'],'request_id':'req-project-safety'},timeout=10)));t.start();token=None
   for _ in range(250):
    events=self.call('status')['result']['events'];matches=[e for e in events if e['kind']=='approval' and e['details'].get('id','').startswith('project-perm-')]
    if matches:token=matches[-1]['details']['id'];break
    if not t.is_alive():break
    time.sleep(.02)
   self.assertIsNotNone(token,result);self.assertEqual(len(calls),1);self.assertNotIn('synthetic owner route',json.dumps(calls));self.assertEqual(self.posts('/sessions'),[])
   approval=[e for e in self.call('status')['result']['events'] if e['kind']=='approval'][-1];self.assertEqual(approval['details']['request_id'],'req-project-safety');self.assertEqual(approval['project_id'],self.project()['id'])
   if mode=='cancel':self.call('cancel')
   elif mode=='switch':
    other=self.folder/'another';other.mkdir();added=self.call('project_add',{'name':'Other','path':str(other)});self.call('assistant_scope',{'project_id':added['result']['project']['id']})
   else:self.assertTrue(self.call('permission_reply',{'id':token,'decision':mode})['ok'])
   t.join(5);self.assertFalse(t.is_alive());self.assertFalse(self.call('permission_reply',{'id':token,'decision':'allow'})['ok'])
   if mode in ('cancel','switch'):
    self.assertFalse(result['value']['ok'],result);self.assertEqual(len(calls),1)
   else:
    self.assertTrue(result['value']['ok'],result);self.assertEqual(result['value']['result']['reply']['source'],'pi-project');self.assertEqual(len(calls),3 if preview else 2);self.assertEqual('synthetic owner route' in json.dumps(calls[1]),mode=='allow');self.assertNotIn('project-perm-',json.dumps(calls));self.assertEqual(result['value']['result']['reply']['actual_model']['id'],'synthetic')
    previews=result['value']['result']['reply'].get('previews',[]);self.assertEqual(len(previews),1 if preview else 0)
    if preview:
     self.assertEqual(previews[0]['before'],'synthetic owner route\n');self.assertEqual(previews[0]['after'],'suggested replacement\n');self.assertIs(previews[0]['applied'],False);self.assertIn('No project file was changed',result['value']['result']['reply']['text']);self.assertEqual(len([e for e in self.call('status')['result']['events'] if e['kind']=='approval']),1)

  finally:provider.shutdown();provider.server_close()
 def test_production_project_harness_with_local_synthetic_provider(self):self.production_harness('allow')
 def test_production_project_harness_denial(self):self.production_harness('deny')
 def test_production_project_harness_cancel(self):self.production_harness('cancel')
 def test_production_project_harness_switch(self):self.production_harness('switch')
 def test_project_slash_never_runs_opencode_command(self):self.production_harness('allow','/new')
 def test_project_persona_text_stays_readonly(self):self.production_harness('allow','be concise')
 def test_production_preview_does_not_write(self):
  self.production_harness('allow',preview=True);self.assertEqual(self.file.read_text(),'synthetic owner route\n')
# Base class has its own tests; run only this route's explicit tests here.
if __name__=='__main__':
 names=[n for n in OwnerProjectTests.__dict__ if n.startswith('test_')];suite=unittest.TestSuite(OwnerProjectTests(n) for n in names);result=unittest.TextTestRunner().run(suite);raise SystemExit(not result.wasSuccessful())
