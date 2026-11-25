# Optional isolated preview syntax validation

The fixed project_validate.py helper parses an exact prepared Python or JSON preview.
It never imports or executes the preview, runs tests or modifies project files.
An owner-only Station control can request this check; no model tool exists.

It revalidates the immutable proposal and runs a fixed ast.parse or strict json.loads program under
Linux bubblewrap with all namespaces unshared, no network, no project mount,
no home, a cleared environment, read-only system runtime mounts and private tmp.
CPU, address space, open files, file size and core dumps are bounded by prlimit.
The process has its own group and a four-second deadline. Missing isolation or
resource-limit support fails closed. There is no unsandboxed fallback.

Result: exact path and proposed SHA256, valid syntax or bounded diagnostic,
applied:false, executed_project_code:false, syntax-only notice. A parser pass
is not a test pass, type check, security review or claim that imports work.

Eight actual Linux-isolated helper tests cover valid syntax, syntax diagnostics,
nonexistent imports and file writes not executed, infinite loop text not run,
wrong extension, changed proposal digest and absent isolation. Synthetic inputs
only. This dependency and behavior still need installed-device acceptance.

Validation is bound to owner-visible exact preview bytes. The owner transport
checks cancellation, EOF and deadline before disclosure. Station shows generic
diagnostics separately from test results. No arbitrary command or shell
capability is implied.

## Exact preview protocol and cancellation

A single-request python -I entry point accepts only the bounded exported preview
schema, revalidates both hashes and rejects extra/duplicate keys. It cannot
accept a root, command, approval, provider or file handle. One JSON line only;
there is no multi-request session. The owner-only pi_preview_validate operation accepts this exact frame.

Cancellation is checked before spawn, while waiting and after child completion.
Stop/deadline kills the isolated process group and reaps it before discarding
the result. Fifteen tests include an actual isolated CLI invocation and injected
child cancellation races/failures. No external project file is opened.

## Optional owner route

NAVI_PROJECT_VALIDATE=1 separately enables Check Python/JSON syntax only beside a
matching preview. Default off. It requires an explicit currently selected project,
only one active check and exact preview bytes/digests. The daemon captures
generation/project epoch and owner socket liveness, rechecks before disclosing
the result and always stops the child. Six-second outer deadline. The fixed
/usr/bin/python3 worker receives a cleared environment. No model RPC interface
or project mount is added. Syntax diagnostics are generic in Station and shown
separately from test results. Eleven actual daemon tests pass plus Chromium owner
click/project/content binding and visible limited-result acceptance.

The helper protocol is a single newline-delimited request per short-lived
process. Extra JSON values on that line are refused; no following request is
processed. Initial integration used a PATH-resolved Python with cleared env,
which could fail interpreter identity. Fixed system interpreter resolves this.
Actual daemon tests using a delayed temporary installed-app fixture verify
in-flight Stop, project switch and owner socket EOF suppress results and reap
the child. A second active check is refused. The production parser executable
is not replaced or made configurable for these tests.

## Strict JSON validation

JSON uses the same isolation/cancellation/owner route, with a fixed standard
library parser, no executable hooks from the project. Duplicate object keys,
NaN/Infinity and float overflow are refused. UTF-8 and null remain data. It is
syntax/strict parse only, not JSON Schema or application validation. Rust checks
that the returned language matches the exact preview extension. Eighteen helper
tests and eleven actual owner-daemon tests pass; Chromium separately renders
Python and JSON controls/results and preserves markup as text.

## JavaScript syntax only

The same owner-only route supports .js/.mjs as ECMAScript modules and .cjs as
a CommonJS function body. The UI names that mode explicitly; .js is not inferred
from project package.json because no project files are read. Fixed /usr/bin/node
uses vm.SourceTextModule or compileFunction to compile only, never evaluate,
link imports or invoke the function. JIT/WebAssembly are disabled. Existing
namespace, environment, resource, cancellation and result binding apply.
TypeScript/JSX and application semantics are not supported. Missing runtime or
flags fails closed. Twenty-two helper tests and twelve actual daemon tests pass.
Synthetic missing imports, infinite loops, throws and require calls stay inert.
Actual Chromium module-mode result inspected.
