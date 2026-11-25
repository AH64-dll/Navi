# Pi project read boundary, preparatory module

`workers/project_read.py` is a read-only filesystem seam with synthetic tests.
It is not wired into Pi, Station, ACP, a model tool or any RPC endpoint yet.
Adding it does not grant a model access to a project.

The intended caller must resolve an owner-selected project and obtain approval
for the exact read/list operation and relative path before calling. Do not take
roots or approvals from model output. Do not enable unrestricted Pi file or
shell tools as a shortcut. A separate transport/approval design and cancellation
checks are required before exposing this module to a model.

## Implemented boundaries

- Opens an absolute project root component by component with `O_NOFOLLOW`.
  Descendant directories and files use pinned directory descriptors and reject
  symlinks. No shell, execution, network, writes or recursive discovery.
- Rejects absolute descendant paths, traversal, hidden components, controls,
  backslashes and a small denylist of sensitive filename stems.
- Reads only an explicit text/code extension allowlist. Files must be regular,
  single-link files, at most 64 KiB, UTF-8 and free of binary controls.
- Bounded output: 200 lines and 16 KiB of text. Metadata changes during the read
  cause failure instead of returning a partly changing file.
- Lists one explicit directory, returning at most 200 entries and examining at
  most 512 entries. No links, devices, hidden or denylisted names are returned.
- Results are labeled untrusted project data. They are never instructions,
  approval, a durable memory source or a reason to run another tool.

Filename filtering cannot guarantee that a code file contains no secret. Exact
owner approval and provider/data handling still matter. Pinning a directory
means a renamed/replaced path does not redirect an existing reader into a new
root; it does not freeze file contents or prevent a project writer from adding
new regular files. The module has no durable cache or model context.

Tests use temporary synthetic projects only:

```
python3 apps/navi/scripts/test_project_read.py
```

Remaining before integration: one-shot per-path owner approval, root/scope
binding, bounded transport, cancellation/close, model-loop budget integration,
privacy review, Station presentation and actual local route acceptance.


## Preparatory approval broker

`workers/project_approvals.py` binds an owner-selected root descriptor and scope
for one bounded broker lifetime. A request produces one random approval ID for
an exact normalized read/list path and line range. It does not read file bytes
before the decision. Only a trusted owner transport may call `decide`; this is
not an authenticated transport itself and is still not exposed as an RPC/tool.

One request is pending at a time; only allow once or deny are accepted. IDs,
scope, deadline and root identity are checked again on decision. The decision
is consumed before a read, including read failures. Close invalidates pending
work. Sixteen request attempts maximum. Caller mutation of the displayed
permission cannot change the approved operation. Owner cancellation and reads
serialize on a lock, so close does not retroactively revoke an already-started
bounded read. A transport must check cancellation before releasing its result.

Ten synthetic broker tests pass. Remaining: authenticated owner decision
transport, model adapter, per-turn lifecycle/cancellation checks and Station UI.
No model route calls this broker.

## Preparatory owner channel

`workers/project_channel.py` separates bounded model request decoding from the
in-process owner decision method. It is not an authenticated transport and is
not wired into a provider or model route. The future daemon must retain
the owner methods and give a model only the request surface. JSONL request frames
are limited to 4096 bytes, reject duplicate keys and nonfinite values, and accept
only operation/arguments, never roots, scopes or approvals. Stream readers must
enforce the byte limit while reading, before allocating the frame.

Cancellation closes the release gate immediately while an already-started read
finishes privately. Cancel and result enqueue have one lock/order. The trusted
nonblocking local queue must tag turns and discard canceled queued events before
model disclosure. A result already delivered cannot be retracted. Queue callbacks
must not reenter the channel. Errors do not forward exception text or absolute
paths. Twelve synthetic channel tests cover allow/deny, replay, malformed frames,
queue failure, bounds, pending cancel and an actual threaded in-flight cancel.

Still required: real authenticated owner connection routing, bounded stream
reader, exact-path Station presentation, turn-tagged downstream cancellation,
provider privacy choice and local end-to-end acceptance. No automatic access to
project files has been enabled.

## Preparatory private-pipe worker

`workers/project_worker.py` is a standalone owner-only subprocess protocol, not
an exposed socket or authenticated remote endpoint. Only the explicit opt-in owner diagnostic route spawns
it. The future daemon must own both pipes, select root/scope itself, and never
connect a model to the control stream. Model proposals are nested request data;
owner decisions are separate control records. Control roots cannot change.

Nine actual subprocess tests use a synthetic temporary project and cover
allow/deny, replay, owner EOF, cancel, duplicate keys, unknown controls, oversized
frames and decision smuggling. Input, output, queue depth and lifetime call counts
are bounded. Output uses a nonblocking pipe with a deadline. The future parent
must enforce a turn deadline and kill/reap the process group on pipe failure; an
input wait or filesystem stall is not an authorization to wait indefinitely.
Cancellation suppresses unsent queued data and closes the worker; pipe bytes
already written still need the daemon's turn gate before any model disclosure.
No provider call, actual owner UI or production permission routing is covered by
these tests.

## Preparatory Rust supervisor

`src/project_host.rs` supervises the real worker with the existing bounded child
pipe/process-group host. It is used by the explicit opt-in owner diagnostic route only.
The root/scope are constructor arguments from the future owner daemon, never
request data. Python runs in isolated mode and imports siblings only from the
worker's own directory. The supervisor enforces exact pending IDs, one-time
allow/deny, scope on all results, sixteen calls and a sixty-second turn deadline
on request/decision/receive. Cancellation stops/reaps the child and discards
queued pipe bytes. The consuming daemon must still check its generation before
disclosing a returned event. A background abandoned host must be dropped or
canceled; deadline enforcement is checked when the supervisor is driven.

Four Rust tests exercise real Python subprocesses with synthetic projects. Full
Rust suite: 55 pass, fresh binary builds. No model project tools or model adapter has been enabled.

## Opt-in owner diagnostic route

With `NAVI_PI_PROJECT_READ=1`, the private local daemon operation
`pi_project_read` resolves a registered owner-selected project ID, accepts one
read/list proposal and emits an exact-path approval through Station's existing
owner permission route. It does not accept a root or approval in proposal data.
Only allow once/deny are offered and accepted. This is a manual diagnostic
operation, not a model tool and not a conversational route yet. By default the
operation is disabled. Results return to the requesting owner connection only,
not to any provider or durable project/model history. The caller must retain
its connection while the owner answers the approval. Owner socket EOF cancels
the pending operation, including a second generation check after receive.
Shutdown terminates the project child before other teardown.

Cancel terminates the project child, invalidates approval IDs and checks the
turn generation again after receiving pipe data. Switching the selected
project also invalidates the pending diagnostic operation. Seven real isolated
daemon tests cover allow, deny, cancel/late decision, root injection, default
disabled state, project switch and owner socket disconnect; no
provider inference is used. Permission dispatch is distinct from result success.
Station's approval renderer recognizes these one-shot IDs, never Always.
Actual Chromium rendering was inspected: the exact path/line range is repeated
beside the approval buttons so scrolling the input cannot hide the scope.

Remaining for full feature: a deliberate owner UI entry point, model adapter/provider privacy configuration and end-to-end
model-loop acceptance. OpenCode remains default.

## Separate bounded Pi project loop

`packages/navi/src/agent-adapter/project-run.ts` now implements a separate
injected-provider Pi agent loop with only project_read/project_list proposals.
It is not yet connected to the owner diagnostic route or a production CLI.
No direct filesystem, root selection, owner decision, shell or write tool is
available. The host supplies the proposal bridge; every bridge call must use
the exact owner-approved read. No provider is selected by default.

Nine offline loop tests cover normalized paths, scope checks, traversal and
control-field rejection, denial latching, result bounds/metadata stripping,
bridge error redaction, cancellation including a bridge ignoring abort, budgets
and already-canceled starts. All 36 runtime tests and three type checks pass.
The denial latch blocks every later proposal in that run. The existing native
Pi tool-free and browser routes are unchanged. Remaining: actual owner bridge
connection, loop CLI, production run wiring, UI entry and end-to-end synthetic
model-to-owner acceptance before a real provider is considered.

## Connected owner proposal bridge and offline end-to-end check

`project-owner.ts` connects only to the private owner daemon socket, checks
owner UID/private runtime permissions, and fixes the owner-selected project ID
in its constructor. The proposal function sends only pi_project_read read/list
requests. It has no generic RPC or permission interface. Abort destroys the
request socket, causing the daemon's owner-EOF cancellation gate to close.
Response framing/bytes/time are bounded and remote exception text is not passed
through to the model. Six bridge tests and all 42 runtime tests pass.

An additional real isolated daemon test runs the actual Pi agent loop with a
synthetic in-process provider, TS socket bridge, Rust supervisor and Python
reader. It waits for a real owner approval before releasing the synthetic file
to the next model turn. Permission IDs and runtime socket paths never enter the
model transcript. Eight owner-route tests pass in total. No real provider was
contacted. A production loop CLI and explicit owner UI route remain to be wired.

## Opt-in conversational local project harness

Station now offers `Pi project (local, approved read-only)` as a separate manual
harness choice. OpenCode stays default. Build with package-station.mjs, set
NAVI_PI_PROJECT_READ=1, NAVI_PROJECT_ENDPOINT=http://127.0.0.1:<port>/v1 and
NAVI_PROJECT_MODEL to the exact locally served model ID. Hosted endpoints,
credentials, query strings, redirects and fallback providers are refused.
No hosted provider was contacted during acceptance. Runtime/project ID are
supplied by the owner daemon, not by the model or prompt. Each run is fresh,
with no cross-harness transcript handoff, session persistence or attachments.

The production CLI, daemon harness route and socket bridge have now passed an
actual end-to-end test against a synthetic local HTTP provider: only one model
request occurs before exact owner approval; the approved synthetic file arrives
in the second request and owner approval IDs never enter provider messages.
Nine real daemon tests, 42 runtime tests, three type checks and Rust 55 tests
pass. The optional bundles build. Five invalid/provider endpoint configurations
are rejected. Actual Chromium dispatch verifies project handoff exclusion and
correct local model metadata. Project switching invalidates the run epoch and
pending reads. A failed owner bridge latches further project proposals.

This is read-only local project parity only. No shell, editing, full Pi/OpenCode
parity or actual installed-device/provider acceptance is claimed.

## Read-only routing acceptance

The project harness now bypasses chief/fast-intent, persona and slash-command
action dispatch. Even an owner prompt phrased as a deletion is passed only to
the read-only project loop, not translated into desktop or shell work. This
is separate from the provider's tool schema: the daemon route itself enforces
the read-only boundary. Approval events carry the originating owner request
and project IDs so Station does not attach them to another active chat row.

Twelve real daemon tests now cover production allow/deny/Stop/project switch.
Stop and project switch leave the local provider at one request and send no file
bytes. An owner-bridge failure aborts the entire loop, so a canceled read cannot
trigger even one more model request using an error result. All 42 runtime tests
and three type checks pass after this change.

## Independent patched-tree acceptance

Fourteen real project-daemon tests pass from an independently applied upstream
tree, including slash/persona-worded prompts staying in the read-only route.
That tree also passes three type checks, Node 214 plus one skip, runtime 42,
Rust 55 and a fresh build. Reader/broker/channel/worker suites and existing
Station host, Pi, ACP, assistant, streaming, Station and graph suites pass.
Dependencies are shared for this acceptance; source files and patch hashes are
independently verified. Actual installed-user hardware remains untested.
