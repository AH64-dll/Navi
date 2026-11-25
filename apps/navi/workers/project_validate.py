"""Parse one already-approved Python edit preview, never run project code.

Optional Linux bubblewrap dependency. Fail closed when isolation is unavailable.
No project directory, home, environment, network, model, shell or apply access.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
sys.path.insert(0, str(Path(__file__).resolve().parent))
from project_edits import EditProposal, propose_edit, ProjectReadError

# This program is fixed, not assembled from project text. Input remains JSON data.
PARSER = '''import ast,json,sys
v=json.load(sys.stdin)
try:
 ast.parse(v["text"], filename="preview.py", mode="exec")
 result={"valid":True}
except SyntaxError as error:
 result={"valid":False,"line":error.lineno,"column":error.offset,"message":str(error.msg)[:200]}
except (ValueError,RecursionError):
 result={"valid":False,"message":"Parser refused the input"}
print(json.dumps(result))
'''


JSON_PARSER = '''import json,sys,math
v=json.load(sys.stdin)
def pairs(items):
 result={}
 for key,value in items:
  if key in result: raise ValueError("Duplicate object key")
  result[key]=value
 return result
def constant(value): raise ValueError("Non-finite number")
def number(value):
 result=float(value)
 if not math.isfinite(result): raise ValueError("Non-finite number")
 return result
try:
 json.loads(v["text"],object_pairs_hook=pairs,parse_constant=constant,parse_float=number)
 result={"valid":True}
except json.JSONDecodeError as error:
 result={"valid":False,"line":error.lineno,"column":error.colno,"message":"Invalid JSON syntax"}
except (ValueError,RecursionError):
 result={"valid":False,"message":"JSON requires unique keys, finite numbers and bounded nesting"}
print(json.dumps(result))
'''


JS_PARSER = r'''const fs=require('node:fs'),vm=require('node:vm');
const v=JSON.parse(fs.readFileSync(0,'utf8'));let result;
try {
 if(v.mode==='commonjs')vm.compileFunction(v.text.replace(/^#![^\r\n]*/,''),['exports','require','module','__filename','__dirname']);
 else new vm.SourceTextModule(v.text,{identifier:'preview.mjs'});
 result={valid:true};
}catch(error){if(!(error instanceof SyntaxError))throw error;result={valid:false,message:'Invalid JavaScript syntax'};}
process.stdout.write(JSON.stringify(result));
'''


def _terminate(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    process.communicate()


def validate_preview(proposal, *, cancel=None):
    if cancel is not None and cancel.is_set():
        raise ProjectReadError('Isolated parser canceled')
    if type(proposal) is not EditProposal:
        raise ProjectReadError('An exact prepared preview is required')
    rebuilt = propose_edit(proposal.path, proposal.before_sha256, proposal.before, proposal.after)
    extension = Path(proposal.path).suffix
    if rebuilt != proposal or extension not in ('.py', '.json', '.js', '.mjs', '.cjs'):
        raise ProjectReadError('Only exact supported syntax previews can be parsed')
    kind = 'python-syntax-only' if extension == '.py' else 'json-syntax-only' if extension == '.json' else 'javascript-syntax-only'
    parser = PARSER if extension == '.py' else JSON_PARSER
    javascript = kind == 'javascript-syntax-only'
    # Resolve trusted installed binaries, never from a model-selected directory.
    bwrap = Path('/usr/bin/bwrap')
    python = Path(sys.executable).resolve()
    if not bwrap.is_file() or not Path('/usr/bin/prlimit').is_file() or not python.is_relative_to('/usr'):
        raise ProjectReadError('Isolated Python validation is unavailable')
    command = ['/usr/bin/prlimit', '--cpu=2:2', '--as=268435456:268435456',
               '--fsize=0:0', '--core=0:0', '--nofile=32:32', '--', str(bwrap), '--unshare-all', '--die-with-parent', '--new-session',
               '--cap-drop', 'ALL', '--clearenv', '--ro-bind', '/usr', '/usr']
    for library in ('/lib', '/lib64'):
        if Path(library).exists():
            command.extend(['--ro-bind', library, library])
    command.extend(['--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp', '--chdir', '/tmp', '--'])
    if javascript:
        if not Path('/usr/bin/node').is_file():
            raise ProjectReadError('Isolated JavaScript validation unavailable')
        command.extend(['/usr/bin/node', '--jitless', '--no-expose-wasm',
                        '--experimental-vm-modules', '--disable-warning=ExperimentalWarning', '-e', JS_PARSER])
    else:
        command.extend([str(python), '-I', '-S', '-B', '-W', 'ignore::SyntaxWarning', '-c', parser])
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, env={}, start_new_session=True)
    deadline = time.monotonic() + 4
    payload = json.dumps({'text': proposal.after,'mode':'commonjs' if extension == '.cjs' else 'module'}).encode()
    try:
        while True:
            if cancel is not None and cancel.is_set():
                raise ProjectReadError('Isolated parser canceled')
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProjectReadError('Isolated parser timed out; project code was not run')
            try:
                out, error = process.communicate(payload, timeout=min(.05, remaining))
                break
            except subprocess.TimeoutExpired:
                payload = None  # communicate retains its first input across retries.
        if cancel is not None and cancel.is_set():
            raise ProjectReadError('Isolated parser canceled')
    except BaseException:
        _terminate(process)
        raise
    if process.returncode != 0 or len(out) > 1024 or error:
        raise ProjectReadError('Isolated parser unavailable or exceeded limits')
    try:
        result = json.loads(out)
        if type(result) is not dict or type(result.get('valid')) is not bool:
            raise ValueError()
        if set(result) - {'valid', 'line', 'column', 'message'}:
            raise ValueError()
    except (ValueError, UnicodeError) as error:
        raise ProjectReadError('Invalid isolated parser response') from error
    return {'kind': kind, 'path': proposal.path,
            'after_sha256': proposal.after_sha256, 'applied': False,
            'executed_project_code': False, 'result': result,
            'notice': 'Syntax only. No imports, tests or project code were run.'}


def validate_python_preview(proposal, *, cancel=None):
    """Compatibility wrapper for the Python-only helper."""
    if type(proposal) is not EditProposal or Path(proposal.path).suffix != '.py':
        raise ProjectReadError('Only exact Python previews can be parsed')
    return validate_preview(proposal, cancel=cancel)


def decode_preview_frame(frame):
    """One bounded exact preview JSON. No roots, commands or file reads."""
    if type(frame) is not bytes or len(frame) > 220000 or not frame.endswith(b'\n'):
        raise ProjectReadError('Invalid preview-validation frame')
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError()
            result[key] = value
        return result
    try:
        value = json.loads(frame, object_pairs_hook=pairs)
        keys = {'schema', 'applied', 'requires_fresh_validation', 'path', 'before_sha256',
                'after_sha256', 'before', 'after'}
        if type(value) is not dict or set(value) != keys or value['schema'] != 'navi-edit-preview-v1' or value['applied'] is not False or value['requires_fresh_validation'] is not True:
            raise ValueError()
        proposal = propose_edit(value['path'], value['before_sha256'], value['before'], value['after'])
        if proposal.after_sha256 != value['after_sha256']:
            raise ValueError()
    except (ValueError, TypeError, UnicodeError, RecursionError) as error:
        raise ProjectReadError('Invalid exact preview-validation request') from error
    return proposal


def validate_request(frame, *, cancel=None):
    return validate_preview(decode_preview_frame(frame), cancel=cancel)


def main():
    # This process is single-request. Signal cancellation must not leave a parser.
    import threading
    cancel = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: cancel.set())
    signal.signal(signal.SIGINT, lambda *_: cancel.set())
    try:
        frame = sys.stdin.buffer.readline(220001)
        result = validate_request(frame, cancel=cancel)
        if cancel.is_set():
            return 1
        print(json.dumps(result, ensure_ascii=True), flush=True)
        return 0
    except (ProjectReadError, OSError):
        # Do not reveal parser/system internals or unreviewed source on failure.
        print(json.dumps({'error': 'Isolated preview syntax check unavailable, invalid or canceled',
                          'applied': False, 'executed_project_code': False}), flush=True)
        return 1


if __name__ == '__main__':
    # Supports python -I: add only the trusted installed sibling directory.
    raise SystemExit(main())
