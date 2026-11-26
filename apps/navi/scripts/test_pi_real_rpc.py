"""Optional real-runtime regression. Requires installed Pi, not credentials."""
import json,os,sys,tempfile,threading,time,subprocess
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'workers'))
from pi_rpc import PiRpcClient,command_for_pi,project_session
import argparse
parser=argparse.ArgumentParser(description='Real Pi RPC against a loopback-only, zero-cost fixture. No external provider calls.')
parser.add_argument('--pi', required=True, help='Path to the actual installed Pi executable')
parser.add_argument('--navid', help='Also verify Station using this built daemon')
args=parser.parse_args()
requests=[]
fixture={"status":200,"hang":False}
request_started=threading.Event()
release_hang=threading.Event()
class API(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  requests.append({'path':self.path,'body':body})
  request_started.set()
  if fixture["hang"]:
   release_hang.wait(timeout=10)
   return
  if fixture["status"]!=200:
   self.send_response(fixture["status"]);self.send_header('Content-Type','application/json');self.end_headers()
   self.wfile.write(json.dumps({"error":{"message":"loopback fixture failure","type":"rate_limit_error","code":"rate_limit_exceeded"}}).encode())
   return
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  for delta,finish in [({'role':'assistant','content':'actual Pi, local provider'},None),({},'stop')]:
   item={'id':'local-test','object':'chat.completion.chunk','created':int(time.time()),'model':'fake','choices':[{'index':0,'delta':delta,'finish_reason':finish}]}
   self.wfile.write(('data: '+json.dumps(item)+'\n\n').encode());self.wfile.flush()
  self.wfile.write(('data: '+json.dumps({"id":"local-test","object":"chat.completion.chunk","created":int(time.time()),"model":"fake","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":8,"total_tokens":18}})+'\n\n').encode())
  self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
server=ThreadingHTTPServer(('127.0.0.1',0),API);threading.Thread(target=server.serve_forever,daemon=True).start()
with tempfile.TemporaryDirectory() as root:
 root=Path(root);agent=root/'agent';agent.mkdir();project=root/'project';project.mkdir()
 model={'providers':{'offline':{'baseUrl':f'http://127.0.0.1:{server.server_port}/v1','api':'openai-completions','apiKey':'offline-test-only','models':[{'id':'fake','name':'Local fixture','reasoning':False,'input':['text'],'contextWindow':32000,'maxTokens':1024,'cost':{'input':0,'output':0,'cacheRead':0,'cacheWrite':0}}]}}}
 (agent/'SYSTEM.md').write_text("UNEXPECTED_GLOBAL_SYSTEM_DISCOVERY")
 (agent/'APPEND_SYSTEM.md').write_text("UNEXPECTED_GLOBAL_APPEND_DISCOVERY")
 (project/'AGENTS.md').write_text("UNEXPECTED_PROJECT_CONTEXT_DISCOVERY")
 (agent/'models.json').write_text(json.dumps(model));os.environ['PI_CODING_AGENT_DIR']=str(agent)
 executable=str(Path(args.pi).resolve())
 for i in range(2):
  with project_session(str(root/'sessions'),str(project),'offline/fake') as session:
   client=PiRpcClient(command_for_pi(executable,'offline/fake',session),str(project));client.start()
   try:
    client.configure();print('setup accepted',flush=True)
    print(json.dumps(client.prompt('Reply with the local fixture result.',timeout=20)),flush=True)
   finally:client.stop()
 assert len(requests)==2, requests
 assert all(r['path']=='/v1/chat/completions' for r in requests)
 assert all(not r['body'].get('tools') for r in requests)
 assert "You are Navi" in json.dumps(requests[0]['body']['messages'])
 assert "UNEXPECTED_" not in json.dumps(requests)
 assert len(requests[1]['body']['messages'])>len(requests[0]['body']['messages'])
 assert any(m.get('role')=='assistant' and m.get('content')=='actual Pi, local provider' for m in requests[1]['body']['messages'])
 transcripts=list((root/'sessions').glob('*.jsonl'))
 assert len(transcripts)==1 and transcripts[0].stat().st_mode & 0o777==0o600
 if args.navid:
  os.environ["NAVI_TEST_BINARY"]=str(Path(args.navid).resolve())
  os.environ["NAVI_PI_EXECUTABLE"]=executable
  os.environ["NAVI_PI_MODEL"]="offline/fake"
  os.environ["NAVI_PI_SESSION_DIR"]=str(root/"station-sessions")
  import test_assistant
  case=test_assistant.AssistantTests()
  case.setUp()
  try:
   note=root/"fixture.md"
   note.write_text("bounded attachment fixture", encoding="utf-8")
   for _ in range(2):
    response=case.call("command", {"text":"what is two plus two", "harness":"pi", "attachments":[{"path":str(note)}]},timeout=30)
    assert response["ok"], response
    answer=response["result"]["reply"]
    assert answer["text"]=="actual Pi, local provider",answer
    assert answer["source"]=="pi-rpc" and answer["turn"].startswith("turn-"),answer
    assert answer["finish"]=="stop" and answer["truncated"] is False,answer
    assert answer["actual_model"]=={"providerID":"offline","id":"fake"},answer
    assert answer["usage"]["tokens"]["input"]==10 and answer["usage"]["tokens"]["output"]==8,answer
    assert "cost" not in answer["usage"],answer
   assert not case.posts("/prompt"), "Unexpected OpenCode fallback"
   assert not case.call("status")["result"]["busy"]
   fixture["hang"]=True
   request_started.clear()
   result={}
   def station_prompt():
    result["response"]=case.call("command", {"text":"what is two plus two", "harness":"pi"},timeout=10)
   pending=threading.Thread(target=station_prompt)
   pending.start()
   assert request_started.wait(timeout=5), "Station never reached actual Pi provider"
   assert case.call("cancel")["ok"]
   pending.join(timeout=5)
   assert not pending.is_alive(), "Station did not finish cancellation"
   assert result["response"]["result"]["reply"]["stopped"],result
   assert not case.call("status")["result"]["busy"]
   assert not case.posts("/prompt")
   fixture["hang"]=False
   release_hang.set()
   release_hang.clear()
  finally:case.tearDown()
  assert len(requests)==5,requests
  assert "bounded attachment fixture" in json.dumps(requests[2]["body"]["messages"])
  assert len(requests[3]["body"]["messages"])>len(requests[2]["body"]["messages"])
  assert all(not r["body"].get("tools") for r in requests)
  print("Actual Station daemon + actual Pi round trip, session reuse and hung-provider cancellation passed",flush=True)
 success_count=len(requests)
 fixture["status"]=429
 from pi_rpc import PiError
 client=PiRpcClient(command_for_pi(executable,"offline/fake"),str(project));client.start()
 started=time.monotonic()
 try:
  client.configure()
  try:client.prompt("Fail without retries.",timeout=5)
  except PiError:pass
  else:raise AssertionError("Provider failure was returned as success")
 finally:client.stop()
 assert len(requests)==success_count+1, "Provider retried despite disabled retry setting"
 assert time.monotonic()-started<5, "Provider error was not prompt"
 print("Actual Pi HTTP429 fails with exactly one local request",flush=True)
 fixture["status"]=200
 fixture["hang"]=True
 for stop_mode in ("deadline", "cancel"):
  request_started.clear()
  client=PiRpcClient(command_for_pi(executable,"offline/fake"),str(project));client.start()
  client.configure()
  def cancel_on_request():
   if request_started.wait(timeout=5):client.cancel()
  cancel_thread=None
  if stop_mode=="cancel":
   cancel_thread=threading.Thread(target=cancel_on_request)
   cancel_thread.start()
  started=time.monotonic()
  try:
   try:client.prompt("Hold the local request.",timeout=.3 if stop_mode=="deadline" else 5)
   except PiError as exc:
    assert ("timed out" if stop_mode=="deadline" else "cancelled") in str(exc),exc
   else:raise AssertionError("Hung provider was returned as success")
  finally:
   client.stop()
   if cancel_thread:cancel_thread.join(timeout=6)
  assert client.proc.returncode is not None
  assert time.monotonic()-started<3
  print("Actual Pi hung HTTP request stopped and reaped: "+stop_mode,flush=True)
 release_hang.set()
 print(json.dumps({'request_count':len(requests),'message_counts':[len(r['body']['messages']) for r in requests],'tools_present':[bool(r['body'].get('tools')) for r in requests],'session_files':[str(p.name) for p in (root/'sessions').glob('*.jsonl')]}))
server.shutdown()
