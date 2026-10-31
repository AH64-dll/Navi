# NAVI foundation architecture

Status: foundation development, not a complete AI product. This document supersedes the product/process placement in [the earlier plan](../navi-architecture/NAVI-ARCHITECTURE.md). Its research and executable reference checks remain intact.

## Product boundary

NAVI is a CLI-based agent harness. The extension connects that harness to a user-authorized document in the user's existing browser. It does not start an agent, own model credentials, or replace the browser profile. The foundation has no model calls, planning loop, memory database, screenshots, site tools, or workflow engine.

```text
User terminal -> NAVI CLI / harness port
                       |
                private local socket (POSIX)
                       |
                native connector (relay only)
                       |
                Chrome native messaging
                       |
              MV3 service worker <- trusted popup
                       |
              isolated content script -> current document
```

The user starts the CLI. Chrome starts the connector when the extension calls `connectNative`. The connector attaches to the already-running CLI; it never starts a second agent. A missing CLI is a visible setup error, not permission to spawn another runtime or fall back to TCP.

## Ownership

| Module | Owns | Must not own |
| --- | --- | --- |
| CLI | Terminal commands, future model/provider and task lifecycle | Browser grants or DOM handles |
| Core | Browser port, pending calls, sequencing, deadlines, current binding | Chrome, DOM, Node transport APIs, model SDK |
| Protocol | Versioned types, runtime validation, fixed byte/count limits | Process lifecycle or user approval |
| Transport | Native framing, bounded stream queues, private socket, relay | Planning, page interpretation, authority decisions |
| Worker | Native port, browser-provided identity, trusted grant, revocation, routing | Models, arbitrary filesystem/network access |
| Content script | Bounded DOM projection, local node table, guarded focus/scroll | Native messaging, grant creation, page MAIN-world execution |
| Popup | Explicit attach, capability choice, status, disconnect | Page-rendered approval or persistent consent |

The browser realm lives under `packages/navi/browser/` (`worker/`, `content/`, `popup/`, `platform/`), with
separate DOM type checking. Shared contracts and the CLI live under `packages/navi/src/`
(`protocol/`, `core/`, `transport/`, `cli/`). A single private workspace avoids prematurely coupling
independent packages through a release system. Module boundaries are enforced by checks, not by package
count: `packages/navi/test/architecture.test.ts` fails if the browser realm imports Node, the CLI or the
transport, if the shared realms touch Node/Chrome/DOM, or if the manifest or bundles gain a permission,
an entrypoint or an external import.

## Consent and target identity

Manifest permissions are `activeTab`, `scripting`, and `nativeMessaging`. No debugger, host patterns, static content scripts, storage, tabs, history, cookies, webRequest, or external messaging.

1. User opens the popup on the desired active tab and selects Attach.
2. The worker captures browser-supplied tab/document identity through isolated top-frame injection.
3. Observe is the default capability. Focus and scroll require a separate explicit choice.
4. A grant binds one tab, one document, one connection, and a fresh grant generation.
5. Switching tabs does not move the grant. An action against a nonactive target is rejected; NAVI never activates it silently.
6. Navigation, closure, replacement, lost ports, disconnect, expiry, or worker restart invalidates authority and references. Reattach is explicit; nothing is replayed.

This deliberately stricter NAVI policy is not a claim that Chrome's `activeTab` permission always expires on same-origin navigation. Browser permission lifetime and NAVI document authority are separate.

The model-facing tool adapter must take target authority from core state, not trust model-supplied grant fields. A field named `approved` in an untrusted message is never authorization.

## Observation and action scope

Only the top frame is supported initially. Observation is requested, bounded, and immutable. It is derived from DOM/ARIA, not Chrome's computed accessibility tree. There is no background mutation stream.

Never read input values, passwords, OTPs, cookies, storage, or arbitrary network bodies. Skip excluded and hidden ancestry before collecting descendant text. Strip URL credentials, query, and fragment; paths and ordinary text can still contain sensitive information. Context remains untrusted page data.

Node references belong to one observation and live document. The content script retains actual node references; they are never reconstructed from page IDs or silently rebound to a similar element. A subsequent observation or revocation invalidates the old table.

The action vocabulary is only focus and scroll. Both can trigger page handlers and can be observed by the site. They are not guaranteed side-effect-free. Synthetic DOM operations do not provide trusted user activation or undetectability. No clicks, typing, submitting, navigation, arbitrary evaluation, files, CAPTCHA handling, or security circumvention are hidden behind the abstraction.

After possible dispatch, loss of confirmation means an unknown outcome, not success and not safe automatic retry. Cancellation prevents future work; it cannot undo page effects.

## Protocol and lifecycle

The CLI and worker exchange strict JSON object envelopes. Native streams use a four-byte native-endian length prefix with strict UTF-8. Browser message ports carry the object directly; they do not double-encode it as a JSON string.

Fixed limits: 64 KiB envelope, 16 KiB observation, 100 returned nodes, 200 characters per text/name, 1500 visited elements, one in-flight browser operation. Byte transports check the length before allocating the body. Object-port receivers check their own budget after Chrome has deserialized the value.

Handshake precedes all work. Connection IDs and per-direction sequence numbers reject stale/replayed traffic. Every request has a unique ID, exact arguments, and current binding. Results are correlated with the pending request. Invalid protocol closes the channel. Queues, waits, and idle lifetime are bounded. Reconnect creates a fresh session; interrupted work is not replayed.

Every inbound frame passes the shared strict validator inside the connection layer, so a transport adapter
never needs its own schema copy. Cancellation has one rule: the canceller confirms
`{cancelled:true, alreadyTerminal:false}` and does **not** send a result for the cancelled request; the
requester settles that request locally as cancelled. If the cancelled operation may already have been
dispatched, the requester reports an unknown outcome instead. Exactly one local terminal outcome exists
per request, and a late original result after a cancel acknowledgement is ignored.

For the exact wire variants, see the shared protocol source and contract tests. A wire change must update types, validators, both peers, fixtures, and version compatibility behavior together.

## Why native messaging plus local IPC

Chrome cannot connect directly to a Unix socket. Native messaging is its narrow browser-to-local-process interface. The additional connector is justified here because the user starts and owns the CLI; the older browser-spawned embedded-agent plan solved a different lifecycle.

The default has no TCP/WebSocket listener, debugging endpoint, pairing server, or permanent daemon. Native host registration pins an extension ID. Local socket access relies on private pathname permissions, not on an assertion of peer identity. Same-account processes and root are outside this boundary; neither an origin argument nor a local token is a sandbox against them.

The foundation targets Chromium and POSIX local IPC. Windows named-pipe ACLs, other browsers, installers, signing, and distribution need separate adapters and validation. Unsupported platforms must fail clearly, not silently weaken transport security.

## Extension seams

Future systems attach through explicit interfaces:

- Pi or another model runtime: CLI-side harness adapter; no import into browser/relay/core.
- Page understanding: replace or extend the bounded observation producer, preserving scope and byte contracts.
- Memory: explicit task-side consumer of approved observations; no ambient browsing capture.
- Vision: separately authorized capture capability and bounded result, not an automatic fallback.
- New tools: strict argument/result variants plus worker-side grant enforcement and cancellation tests.
- Multi-tab reasoning: explicit multiple bindings; never redefine active tab as implicit authority.
- Skills, planning, workflows, local models: CLI-side services using the same browser port, not alternate privileged routes.

Add an interface when there is a caller or implementation. Do not precreate empty memory/vision/skills frameworks.

## Required verification

Foundation tests must cover framing, strict schemas, handshake/sequence rules, permission denial, stale references, cancellation/unknown outcomes, queue bounds, DOM exclusions, manifest limits, and import direction. Mock tests and bundle checks do not establish live browser behavior.

Before declaring a browser release ready, manually validate native registration, popup sender identity, top-frame document binding, active-tab checks, navigation/BFCache/discard/replacement, worker restart, idle teardown, and real site focus/scroll behavior. Use local fixtures or authorized test sites without real credentials.
