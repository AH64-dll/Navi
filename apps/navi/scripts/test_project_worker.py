"""Actual subprocess/private-pipe acceptance, synthetic project only."""
from pathlib import Path
import json
import os
import select
import subprocess
import sys
import tempfile
import unittest
WORKER=Path(__file__).resolve().parents[1]/'workers'/'project_worker.py'
class WorkerTests(unittest.TestCase):
 def setUp(self):
  self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name);(self.root/'one.py').write_text('synthetic pipe\n');self.proc=subprocess.Popen([sys.executable,str(WORKER),str(self.root),'turn'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0);self.assertEqual(self.read()['type'],'ready')
 def tearDown(self):
  if self.proc.poll() is None:self.proc.kill()
  self.proc.communicate(timeout=2);self.temp.cleanup()
 def send(self,value):self.proc.stdin.write(json.dumps(value).encode()+b'\n');self.proc.stdin.flush()
 def read(self):
  self.assertTrue(select.select([self.proc.stdout],[],[],2)[0],'worker response timeout');line=self.proc.stdout.readline();self.assertTrue(line,'worker closed');return json.loads(line)
 def request(self):self.send({'type':'request','request':{'operation':'read','args':{'path':'one.py'}}});return self.read()['id']
 def test_actual_owner_pipe_allow(self):
  token=self.request();self.send({'type':'decision','id':token,'decision':'allow'});self.assertEqual(self.read()['result']['text'],'synthetic pipe\n')
 def test_deny(self):
  token=self.request();self.send({'type':'decision','id':token,'decision':'deny'});self.assertTrue(self.read()['result']['denied'])
 def test_cancel_is_terminal(self):
  self.request();self.send({'type':'cancel'});self.proc.wait(timeout=2);self.assertEqual(self.proc.stdout.read(),b'')
 def test_eof_reaps_worker(self):
  self.request();self.proc.stdin.close();self.proc.stdin=None;self.assertEqual(self.proc.wait(timeout=2),0)
 def test_model_cannot_smuggle_control(self):
  self.send({'type':'request','request':{'operation':'read','args':{'path':'one.py'},'decision':'allow'}});self.assertNotEqual(self.proc.wait(timeout=2),0);self.assertEqual(self.proc.stdout.read(),b'')
 def test_duplicate_control_keys_rejected(self):
  self.proc.stdin.write(b'{"type":"cancel","type":"request"}\n');self.proc.stdin.flush();self.assertNotEqual(self.proc.wait(timeout=2),0)
 def test_flood_and_truncated_frame_exit(self):
  self.proc.stdin.write(b'x'*8193);self.proc.stdin.flush();self.assertNotEqual(self.proc.wait(timeout=2),0)
 def test_replayed_decision_closes_without_second_result(self):
  token=self.request();v={'type':'decision','id':token,'decision':'allow'};self.send(v);self.read();self.send(v);self.proc.wait(timeout=2);self.assertEqual(self.proc.stdout.read(),b'')
 def test_unknown_control_is_terminal(self):
  self.send({'type':'root','path':'/'});self.assertNotEqual(self.proc.wait(timeout=2),0);self.assertEqual(self.proc.stderr.read(),b'')
if __name__=='__main__':unittest.main()
