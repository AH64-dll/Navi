"""Synthetic preparatory channel tests, no provider or private project access."""
from pathlib import Path
import json
import sys
import tempfile
import threading
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'workers'))
from project_channel import ProjectChannel, ProjectReadError, decode_request

class ChannelTests(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);(self.root/'one.py').write_text('synthetic\n');self.events=[];self.channel=ProjectChannel(self.root,'turn',self.events.append)
 def tearDown(self):self.channel.close();self.temp.cleanup()
 def request(self):self.channel.request(b'{"operation":"read","args":{"path":"one.py"}}\n');return self.events[-1]['id']
 def test_request_only_emits_permission(self):
  token=self.request();self.assertEqual(len(self.events),1);self.assertEqual(self.events[0]['type'],'permission');self.assertNotIn('result',self.events[0]);self.channel.owner_decide(token,'allow');self.assertEqual(self.events[-1]['result']['text'],'synthetic\n')
 def test_owner_decisions_not_model_frames(self):
  for value in [{'operation':'read','args':{'path':'one.py'},'decision':'allow'},{'root':str(self.root),'operation':'list','args':{'path':''}},{'id':'x','decision':'allow'}]:
   with self.assertRaises(ProjectReadError):self.channel.request(json.dumps(value).encode()+b'\n')
  self.assertEqual(self.events,[])
 def test_bounded_duplicate_or_invalid_frames(self):
  for frame in [b'{}',b'{}\n{}\n',b'{"operation":"read","operation":"list","args":{}}\n',b'x'*4097+b'\n',b'{"operation":NaN,"args":{}}\n',b'\xff\n',b'['*1500+b']'*1500+b'\n']:
   with self.assertRaises(ProjectReadError):decode_request(frame)
 def test_deny_and_no_replay(self):
  token=self.request();self.channel.owner_decide(token,'deny');self.assertTrue(self.events[-1]['result']['denied'])
  with self.assertRaises(ProjectReadError):self.channel.owner_decide(token,'allow')
 def test_wrong_token_or_always_cannot_read(self):
  token=self.request()
  for ident,decision in [('wrong','allow'),(token,'always')]:
   with self.assertRaises(ProjectReadError):self.channel.owner_decide(ident,decision)
  self.assertEqual(len(self.events),1);self.channel.owner_decide(token,'deny')
 def test_cancel_pending(self):
  token=self.request();self.channel.cancel()
  with self.assertRaises(ProjectReadError):self.channel.owner_decide(token,'allow')
  self.assertEqual(len(self.events),1)
 def test_cancel_inflight_drops_result(self):
  token=self.request();started=threading.Event();release=threading.Event();original=self.channel._broker.reader.read
  def blocked(**kw):started.set();release.wait(2);return original(**kw)
  self.channel._broker.reader.read=blocked
  worker=threading.Thread(target=self.channel.owner_decide,args=(token,'allow'));worker.start();self.assertTrue(started.wait(1));self.channel.cancel();release.set();worker.join(2);self.assertFalse(worker.is_alive());self.assertEqual(len(self.events),1);self.assertTrue(self.channel._broker.closed)
 def test_failure_redacts_absolute_path(self):
  token=self.request();(self.root/'one.py').unlink();self.channel.owner_decide(token,'allow');self.assertEqual(self.events[-1]['type'],'project_error');self.assertNotIn(str(self.root),json.dumps(self.events[-1]))
 def test_busy_and_pending_reject_overlap(self):
  self.request()
  with self.assertRaises(ProjectReadError):self.request()
 def test_owner_queue_failure_closes_channel(self):
  def fail(event):raise RuntimeError('queue closed')
  self.channel._emit=fail
  with self.assertRaises(RuntimeError):self.request()
  self.assertTrue(self.channel._broker.closed)
 def test_result_limit_and_escaped_output(self):
  token=self.request();self.channel._broker.reader.read=lambda **kw:{'text':'x'*262145};self.channel.owner_decide(token,'allow');self.assertEqual(self.events[-1]['type'],'project_error')
 def test_closed_cannot_reopen(self):
  self.channel.cancel()
  with self.assertRaises(ProjectReadError):self.request()
if __name__=='__main__':unittest.main()
