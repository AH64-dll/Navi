"""Actual daemon -> ACP Python transport -> deterministic ACP subprocess."""
import os,json,tempfile,threading,time,unittest,sys
from pathlib import Path
import test_assistant
from test_acp_client import FAKE_AGENT
class AcpStationTests(test_assistant.AssistantTests):
 def setUp(self):
  self.bundle=tempfile.TemporaryDirectory();self.agent=Path(self.bundle.name)/'agent.py';self.log=Path(self.bundle.name)/'log.jsonl';self.agent.write_text(FAKE_AGENT.replace('elif text == "permission":','elif text.rstrip().endswith("permission"):'))
  self.old=os.environ.get('NAVI_ACP_CMD_CLAUDE');os.environ['NAVI_ACP_CMD_CLAUDE']=f'{sys.executable} {self.agent} {self.log}'
  super().setUp()
 def tearDown(self):
  super().tearDown()
  if self.old is None:os.environ.pop('NAVI_ACP_CMD_CLAUDE',None)
  else:os.environ['NAVI_ACP_CMD_CLAUDE']=self.old
  self.bundle.cleanup()
 def test_owner_sigkill_reaps_acp_agent(self):
  import signal,subprocess
  pidfile=self.folder/'acp-agent.pid'
  self.agent.write_text('import os,time\nopen('+repr(str(pidfile))+',"w").write(str(os.getpid()))\ntime.sleep(60)\n')
  def run():
   try:self.call('command',{'text':'permission','harness':'claude'})
   except (OSError,ValueError):pass
  thread=threading.Thread(target=run);thread.start()
  for _ in range(100):
   if pidfile.exists():break
   time.sleep(.02)
  self.assertTrue(pidfile.exists());pid=int(pidfile.read_text())
  self.process.kill();self.process.wait(3);thread.join(3)
  for _ in range(150):
   try:os.kill(pid,0)
   except ProcessLookupError:break
   time.sleep(.02)
  else:
   os.kill(pid,signal.SIGKILL);self.fail('ACP agent survived daemon SIGKILL')
 def test_missing_allow_once_never_becomes_persistent(self):
  self.agent.write_text(self.agent.read_text().replace('{"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},',''))
  result={};thread=threading.Thread(target=lambda:result.update(reply=self.call('command',{'text':'permission','harness':'claude'})));thread.start()
  approval=None
  for _ in range(150):
   events=self.call('status')['result']['events'];approval=next((e for e in reversed(events) if e['kind']=='approval' and e['details'].get('id','').startswith('acp-perm-')),None)
   if approval:break
   time.sleep(.02)
  self.assertIsNotNone(approval);self.assertNotIn('allow',approval['details']['choices'])
  self.assertFalse(self.call('permission_reply',{'id':approval['details']['id'],'decision':'allow'})['ok'])
  thread.join(5);self.assertFalse(thread.is_alive());self.assertTrue(result['reply']['ok'])
  entries=[json.loads(line) for line in self.log.read_text().splitlines()]
  replies=[e['msg']['result']['outcome'] for e in entries if e['dir']=='reply' and e.get('msg',{}).get('result',{}).get('outcome')]
  self.assertEqual(replies[-1],{'outcome':'cancelled'})
 def test_cancel_invalidates_pending_owner_decision(self):
  result={};thread=threading.Thread(target=lambda:result.update(reply=self.call('command',{'text':'permission','harness':'claude'})));thread.start()
  approval=None
  for _ in range(150):
   events=self.call('status')['result']['events'];approval=next((e for e in reversed(events) if e['kind']=='approval' and e['details'].get('id','').startswith('acp-perm-')),None)
   if approval:break
   time.sleep(.02)
  self.assertIsNotNone(approval);self.assertTrue(self.call('cancel')['ok'])
  thread.join(5);self.assertFalse(thread.is_alive());self.assertFalse(result['reply']['ok'])
  self.assertFalse(self.call('permission_reply',{'id':approval['details']['id'],'decision':'allow'})['ok'])
 def run_decision(self,decision):
  result={};thread=threading.Thread(target=lambda:result.update(reply=self.call('command',{'text':'permission','harness':'claude'})))
  thread.start();approval=None
  for _ in range(150):
   events=self.call('status')['result']['events'];approval=next((e for e in reversed(events) if e['kind']=='approval' and e['details'].get('id','').startswith('acp-perm-')),None)
   if approval:break
   time.sleep(.02)
  self.assertIsNotNone(approval,{'result':result,'events':events,'log':self.log.read_text() if self.log.exists() else 'none'})
  ident=approval['details']['id'];self.assertIn('allow',approval['details']['choices']);self.assertIn('always',approval['details']['choices'])
  self.assertTrue(self.call('permission_reply',{'id':ident,'decision':decision})['ok'])
  thread.join(5);self.assertFalse(thread.is_alive());self.assertTrue(result['reply']['ok'],result)
  self.assertFalse(self.call('permission_reply',{'id':ident,'decision':'allow'})['ok'])
  self.assertFalse(self.posts('/permissions/'+ident+'/reply'))
  entries=[json.loads(line) for line in self.log.read_text().splitlines()]
  replies=[e['msg']['result']['outcome'] for e in entries if e['dir']=='reply' and e.get('msg',{}).get('result',{}).get('outcome')]
  expected={'allow':'allow-once','always':'always','deny':'reject-once'}[decision]
  self.assertEqual(replies[-1],{'outcome':'selected','optionId':expected})
 def test_allow_once(self):self.run_decision('allow')
 def test_always_only_when_offered(self):self.run_decision('always')
 def test_deny(self):self.run_decision('deny')
def load_tests(loader,tests,pattern):return unittest.TestSuite([AcpStationTests(name) for name in ['test_allow_once','test_always_only_when_offered','test_deny','test_missing_allow_once_never_becomes_persistent','test_cancel_invalidates_pending_owner_decision','test_owner_sigkill_reaps_acp_agent']])
if __name__=='__main__':unittest.main()
