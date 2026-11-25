"""Actual namespace-isolated Python parser, synthetic in-memory previews only."""
from pathlib import Path
from dataclasses import replace
import hashlib
import sys
import threading
import subprocess
import json
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from project_edits import propose_edit,ProjectReadError
from project_validate import validate_python_preview,validate_request,validate_preview

def proposal(after,path='synthetic.py'):
 before='pass\n'
 return propose_edit(path,hashlib.sha256(before.encode()).hexdigest(),before,after)
class ValidationTests(unittest.TestCase):
 def test_valid_syntax_not_execution(self):
  result=validate_python_preview(proposal('raise RuntimeError("must never execute")\n'))
  self.assertTrue(result['result']['valid']);self.assertFalse(result['executed_project_code']);self.assertFalse(result['applied'])
 def test_imports_and_file_actions_are_only_parsed(self):
  result=validate_python_preview(proposal('import no_such_module\nopen("/not-a-real-file", "w").write("never")\n'))
  self.assertTrue(result['result']['valid'])
 def test_syntax_error_bound_to_exact_proposal(self):
  p=proposal('def broken(\n');result=validate_python_preview(p)
  self.assertFalse(result['result']['valid']);self.assertEqual(result['result']['line'],1);self.assertEqual(result['after_sha256'],p.after_sha256)
 def test_no_import_or_loop_execution(self):
  self.assertTrue(validate_python_preview(proposal('while True: pass\n'))['result']['valid'])
 def test_only_python(self):
  with self.assertRaises(ProjectReadError):validate_python_preview(proposal('hello','x.ts'))
 def test_tampered_after_digest_refused(self):
  with self.assertRaises(ProjectReadError):validate_python_preview(replace(proposal('print(1)\n'),after_sha256='0'*64))
 def test_missing_isolation_fails_closed(self):
  with patch('project_validate.Path.is_file',return_value=False):
   with self.assertRaises(ProjectReadError):validate_python_preview(proposal('print(1)\n'))
 def test_dict_cannot_forge_prepared_proposal(self):
  with self.assertRaises(ProjectReadError):validate_python_preview({'path':'x.py'})
 def test_pre_cancel_never_spawns(self):
  cancel=threading.Event();cancel.set()
  with patch('project_validate.subprocess.Popen') as spawn:
   with self.assertRaisesRegex(ProjectReadError,'canceled'):validate_python_preview(proposal('print(1)\n'),cancel=cancel)
   spawn.assert_not_called()
 def test_inflight_cancel_reaps_child_and_suppresses_result(self):
  cancel=threading.Event()
  class Child:
   pid=123456;returncode=None
   def poll(self):return self.returncode
   def __init__(self):self.calls=0
   def communicate(self,*args,**kwargs):
    self.calls+=1
    if self.calls==1:cancel.set();raise subprocess.TimeoutExpired('fixed',.05)
    return (b'',b'')
  child=Child()
  with patch('project_validate.subprocess.Popen',return_value=child),patch('project_validate.os.killpg') as kill:
   with self.assertRaisesRegex(ProjectReadError,'canceled'):validate_python_preview(proposal('print(1)\n'),cancel=cancel)
   kill.assert_called_once();self.assertEqual(child.calls,2)
 def test_cancel_at_completion_drops_valid_result(self):
  cancel=threading.Event()
  class Child:
   pid=123456;returncode=0
   def poll(self):return self.returncode
   def communicate(self,*args,**kwargs):cancel.set();return (b'{"valid":true}',b'')
  with patch('project_validate.subprocess.Popen',return_value=Child()),patch('project_validate.os.killpg',side_effect=ProcessLookupError):
   with self.assertRaisesRegex(ProjectReadError,'canceled'):validate_python_preview(proposal('print(1)\n'),cancel=cancel)
 def test_parser_failure_does_not_fallback(self):
  class Child:
   returncode=1
   def communicate(self,*args,**kwargs):return (b'',b'namespace denied')
  with patch('project_validate.subprocess.Popen',return_value=Child()) as spawn:
   with self.assertRaisesRegex(ProjectReadError,'unavailable'):validate_python_preview(proposal('print(1)\n'))
   self.assertEqual(spawn.call_count,1)
 def request(self):
  p=proposal('raise RuntimeError("not executed")\n')
  return {'schema':'navi-edit-preview-v1','applied':False,'requires_fresh_validation':True,'path':p.path,'before_sha256':p.before_sha256,'after_sha256':p.after_sha256,'before':p.before,'after':p.after}
 def test_exact_copy_json_and_actual_isolated_cli(self):
  frame=(json.dumps(self.request())+'\n').encode();self.assertTrue(validate_request(frame)['result']['valid'])
  result=subprocess.run([sys.executable,'-I',str(Path(__file__).resolve().parents[1]/'workers/project_validate.py')],input=frame,capture_output=True,timeout=5)
  self.assertEqual(result.returncode,0,result.stderr);self.assertFalse(json.loads(result.stdout)['executed_project_code'])
 def test_unknown_fields_decisions_and_changed_content_refused(self):
  for field,value in [('root','/'),('command','run'),('decision','allow'),('after','changed')]:
   request=self.request();request[field]=value
   with self.assertRaises(ProjectReadError):validate_request((json.dumps(request)+'\n').encode())
 def test_duplicate_keys_and_frame_limits(self):
  for frame in [b'{}',b'{}\n{}\n',b'x'*220001+b'\n',b'{"schema":1,"schema":2}\n']:
   with self.assertRaises(ProjectReadError):validate_request(frame)
 def test_json_valid_and_no_execution(self):
  result=validate_preview(proposal('{"command":"delete", "markup":"<script>never</script>"}','x.json'))
  self.assertTrue(result['result']['valid']);self.assertEqual(result['kind'],'json-syntax-only');self.assertFalse(result['executed_project_code'])
 def test_json_invalid_duplicate_and_nonfinite(self):
  for after in ['{"a":1,"a":2}','NaN','Infinity','1e999','{"a":}', '[1,]']:
   self.assertFalse(validate_preview(proposal(after,'x.json'))['result']['valid'])
 def test_json_unicode_and_null_stay_data(self):
  self.assertTrue(validate_preview(proposal('{"مرحبا":null,"emoji":"😀"}','x.json'))['result']['valid'])
 def test_javascript_modules_not_linked_or_executed(self):
  for extension in ['.js','.mjs']:
   result=validate_preview(proposal('import x from "nonexistent"; throw Error("never execute"); await new Promise(()=>{});','x'+extension))
   self.assertTrue(result['result']['valid']);self.assertEqual(result['kind'],'javascript-syntax-only');self.assertFalse(result['executed_project_code'])
 def test_commonjs_wrapper_never_invoked(self):
  result=validate_preview(proposal('require("nonexistent"); module.exports=1; while(true){}; return;','x.cjs'))
  self.assertTrue(result['result']['valid'])
 def test_invalid_javascript_and_typescript_refused(self):
  for text in ['const = ;','const value: number = 1;']:
   self.assertFalse(validate_preview(proposal(text,'x.mjs'))['result']['valid'])
 def test_commonjs_cannot_import_module_syntax(self):
  self.assertFalse(validate_preview(proposal('import x from "anything";','x.cjs'))['result']['valid'])
 def test_commonjs_hashbang_is_loader_compatible(self):
  self.assertTrue(validate_preview(proposal('#!/usr/bin/env node\nmodule.exports=1;','x.cjs'))['result']['valid'])
 def test_reaped_process_is_not_signaled(self):
  from project_validate import _terminate
  class Child:
   pid=123456
   def poll(self):return 0
   def communicate(self):return b'',b''
  with patch('project_validate.os.killpg') as kill:_terminate(Child());kill.assert_not_called()
if __name__=='__main__':unittest.main()
