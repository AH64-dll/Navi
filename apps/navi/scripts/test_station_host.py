"""Station daemon permission routing using a deterministic local host subprocess."""
import os,json,tempfile,threading,time,unittest
from pathlib import Path
import test_assistant
FAKE='''import readline from 'node:readline';
const emit=v=>console.log(JSON.stringify(v));
emit({type:'ready',mode:'fixture'});
for await(const line of readline.createInterface({input:process.stdin})){
 const m=JSON.parse(line);
 if(m.type==='prompt'){if(typeof m.scope!=='string'||!m.historyText)throw Error('missing owner history metadata');if(!m.text.includes('[project '))throw Error('missing project context');emit({type:'permission',id:'navi-perm-fixture',tool:'navi_focus',input:{ref:'node'},choices:['allow','deny']});}
 if(m.type==='permission_reply'){emit({type:'permission_result',id:m.id,replied:true});emit({type:'done',text:'approved fixture',source:'pi-station',usage:{tokens:{input:22,output:14,cache_read:4,cache_write:2}}});}
 if(m.type==='cancel')emit({type:'error',text:'cancelled'});
}
'''
class HostTests(test_assistant.AssistantTests):
 def setUp(self):
  self.bundle=tempfile.TemporaryDirectory();self.path=Path(self.bundle.name)/'host.mjs';self.path.write_text(FAKE)
  self.old=os.environ.get('NAVI_STATION_HOST_BUNDLE');os.environ['NAVI_STATION_HOST_BUNDLE']=str(self.path)
  super().setUp()
 def tearDown(self):
  super().tearDown()
  if self.old is None:os.environ.pop('NAVI_STATION_HOST_BUNDLE',None)
  else:os.environ['NAVI_STATION_HOST_BUNDLE']=self.old
  self.bundle.cleanup()
 def test_existing_approval_route_reaches_browser_host_once_not_opencode(self):
  self.assertTrue(self.call('pi_browser_start')['ok'])
  result={}
  t=threading.Thread(target=lambda:result.update(reply=self.call('command',{'text':'what is two plus two','harness':'pi-browser'})))
  t.start()
  for _ in range(100):
   events=self.call('status')['result']['events']
   if any(e['kind']=='approval' for e in events):break
   time.sleep(.02)
  self.assertFalse(self.call('permission_reply',{'id':'navi-perm-fixture','decision':'always'})['ok'])
  self.assertTrue(self.call('permission_reply',{'id':'navi-perm-fixture','decision':'allow'})['ok'])
  t.join(timeout=5);self.assertFalse(t.is_alive());self.assertTrue(result['reply']['ok'],result)
  self.assertFalse(self.call('permission_reply',{'id':'navi-perm-fixture','decision':'allow'})['ok'])
  self.assertFalse(self.posts('/permissions/navi-perm-fixture/reply'))
  self.assertEqual(result['reply']['result']['reply']['text'],'approved fixture')
  self.assertEqual(result['reply']['result']['reply']['usage']['tokens']['input'],22)
  self.assertNotIn('cost',result['reply']['result']['reply']['usage'])
 def test_stream_flood_is_bounded_and_clears_stale_host(self):
  self.path.write_text("import readline from 'node:readline';console.log(JSON.stringify({type:'ready'}));for await(const line of readline.createInterface({input:process.stdin})){for(let i=0;i<5000;i++){console.log(JSON.stringify({type:'delta',text:'x'.repeat(1000)}));await new Promise(r=>setTimeout(r,1));}}")
  self.assertTrue(self.call('pi_browser_start')['ok'])
  result=self.call('command',{'text':'what is two plus two','harness':'pi-browser'})
  self.assertFalse(result['ok']);self.assertIn('limit',result['error'])
  self.path.write_text(FAKE)
  self.assertTrue(self.call('pi_browser_start')['ok'])
  self.assertTrue(self.call('pi_browser_stop')['ok'])
 def test_no_attached_browser_rejection_does_not_poison_next_start(self):
  self.path.write_text("import readline from 'node:readline';console.log(JSON.stringify({type:'ready'}));for await(const line of readline.createInterface({input:process.stdin})){console.log(JSON.stringify({type:'control_rejected'}));}")
  self.assertTrue(self.call('pi_browser_start')['ok'])
  self.assertFalse(self.call('command',{'text':'what is two plus two','harness':'pi-browser'})['ok'])
  self.path.write_text(FAKE)
  self.assertTrue(self.call('pi_browser_start')['ok'])
  self.assertTrue(self.call('pi_browser_stop')['ok'])
 def test_explicit_text_attachment_is_framed_and_cleaned(self):
  self.path.write_text("import readline from 'node:readline';console.log(JSON.stringify({type:'ready'}));for await(const line of readline.createInterface({input:process.stdin})){const m=JSON.parse(line);if(m.historyText!==undefined)throw Error('attachment request entered history');if(typeof m.scope!=='string')throw Error('missing scope');if(!m.text.includes('untrusted_text')||!m.text.includes('attachment fixture'))throw Error('missing attachment');console.log(JSON.stringify({type:'done',text:'text attached',source:'pi-station'}));}")
  self.assertTrue(self.call('pi_browser_start')['ok'])
  note=self.folder/'note.txt';note.write_text('attachment fixture')
  reply=self.call('command',{'text':'what is two plus two','harness':'pi-browser','attachments':[{'path':str(note)}]})
  self.assertTrue(reply['ok'],reply);self.assertEqual(reply['result']['reply']['text'],'text attached')
  self.assertEqual(note.read_text(),'attachment fixture');self.assertEqual(list(self.runtime.glob('attach-*')),[])
 def test_invalid_attachment_never_reaches_host_and_is_cleaned(self):
  self.assertTrue(self.call('pi_browser_start')['ok'])
  for name,data in [('image.png',b'bad'),('large.txt',b'x'*8193),('binary.txt',b'a\x00b'),('bad.txt',b'\xff')]:
   note=self.folder/name;note.write_bytes(data)
   reply=self.call('command',{'text':'what is two plus two','harness':'pi-browser','attachments':[{'path':str(note)}]})
   self.assertFalse(reply['ok'],reply);self.assertEqual(list(self.runtime.glob('attach-*')),[])
   self.assertTrue(self.call('pi_browser_start')['ok'])
 def test_readiness_status_tracks_attach_and_hides_private_identifiers(self):
  self.assertFalse(self.call('pi_browser_status')['result']['running'])
  self.path.write_text("console.log(JSON.stringify({type:'status',link:'ready',grant:{grantId:'private-id',binding:{documentId:'private-doc'}}}));console.log(JSON.stringify({type:'ready'}));setInterval(()=>{},1000)")
  self.assertTrue(self.call('pi_browser_start')['ok'])
  status=self.call('pi_browser_status')['result'];self.assertTrue(status['running']);self.assertTrue(status['attached']);self.assertNotIn('private',json.dumps(status))
  self.call('pi_browser_stop');self.assertFalse(self.call('pi_browser_status')['result']['running'])
 def test_startup_failure_preserves_actionable_setup_message(self):
  self.path.write_text("console.log(JSON.stringify({type:'startup_error',text:'Set local model before starting'}))")
  result=self.call('pi_browser_start');self.assertFalse(result['ok']);self.assertIn('Set local model',result['error'])
def load_tests(loader,tests,pattern):return unittest.TestSuite([HostTests(name) for name in ['test_existing_approval_route_reaches_browser_host_once_not_opencode','test_stream_flood_is_bounded_and_clears_stale_host','test_no_attached_browser_rejection_does_not_poison_next_start','test_explicit_text_attachment_is_framed_and_cleaned','test_invalid_attachment_never_reaches_host_and_is_cleaned','test_readiness_status_tracks_attach_and_hides_private_identifiers','test_startup_failure_preserves_actionable_setup_message']])
if __name__=='__main__':unittest.main()
