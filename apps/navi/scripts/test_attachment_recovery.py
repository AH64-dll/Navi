"""SIGKILL recovery removes exact registered snapshots, never prefix lookalikes."""
import base64,json,os,signal,sqlite3,subprocess,threading,time,unittest
import test_pi_station,test_assistant
class RecoveryTests(test_pi_station.NativePiTests):
 def test_sigkill_then_restart_cleans_owned_snapshot_preserving_user_files(self):
  pidfile=self.folder/'provider.pid'
  self.fake.write_text('#!/usr/bin/python3\nimport json,os,sys,time\nopen('+repr(str(pidfile))+',"w").write(str(os.getpid()))\njson.loads(sys.stdin.buffer.readline())\ntime.sleep(60)\n')
  user=self.runtime/'attach-user-note.txt';user.write_text('preserve')
  results={}
  def run():
   try:results['result']=self.call('command',{'text':'what is two plus two','harness':'pi','attachments':[{'name':'private.txt','data':base64.b64encode(b'private fixture').decode()}]})
   except (OSError,ValueError):pass
  worker=threading.Thread(target=run);worker.start()
  rows=[]
  for _ in range(100):
   with sqlite3.connect(self.data/'station.sqlite3') as db:rows=db.execute('select path from attachment_snapshots').fetchall()
   if rows and pidfile.exists():break
   time.sleep(.02)
  self.assertEqual(len(rows),1);snapshot=rows[0][0];self.assertTrue(os.path.isfile(snapshot))
  self.process.kill();self.process.wait(3);worker.join(3)
  # Owner EOF must reap the provider itself, not rely on test cleanup.
  pid=int(pidfile.read_text())
  for _ in range(150):
   try:os.kill(pid,0)
   except ProcessLookupError:break
   time.sleep(.02)
  else:
   os.kill(pid,signal.SIGKILL)
   self.fail('provider survived owner daemon SIGKILL')
  self.fake.write_text(test_pi_station.FAKE)
  self.process=subprocess.Popen([str(test_assistant.BINARY),'serve'],env=self.environment,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  for _ in range(150):
   try:
    if self.call('ping')['ok']:break
   except OSError:pass
   time.sleep(.02)
  self.assertFalse(os.path.exists(snapshot));self.assertEqual(user.read_text(),'preserve')
  with sqlite3.connect(self.data/'station.sqlite3') as db:self.assertEqual(db.execute('select count(*) from attachment_snapshots').fetchone()[0],0)
def load_tests(loader,tests,pattern):return unittest.TestSuite([RecoveryTests('test_sigkill_then_restart_cleans_owned_snapshot_preserving_user_files')])
if __name__=='__main__':unittest.main()
