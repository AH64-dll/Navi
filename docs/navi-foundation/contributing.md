# NAVI foundation contributor guide

This guide is for changes to `packages/navi` and `docs/navi-foundation`. Read the
[architecture](architecture.md) first: it defines the product boundary, the consent model and what is
deferred. Both documents describe the foundation, not a finished assistant.

## Ownership map

| Path | Realm | Rules |
| --- | --- | --- |
| `packages/navi/src/protocol/**` | browser-safe | No Node, Chrome or DOM. Types, strict validators and fixed limits. |
| `packages/navi/src/core/**` | browser-neutral | No Node, Chrome or DOM. Ports, connection state, grants, CLI client. |
| `packages/navi/src/transport/**` | Node | Framing, bounded channels, POSIX socket, relay connector. |
| `packages/navi/src/cli/**` | Node | `serve`, diagnostics, native setup, terminal-safe output. |
| `packages/navi/browser/**` | DOM + Chrome | Never imported by `src/`. Own tsconfig with `DOM` lib. |
| `packages/navi/test/**` | Node | `node:test` suites for the Node realm. |
| `packages/navi/browser/test/**` | DOM-typed | `node:test` with fake platform/DOM fixtures. |

One-way dependency: `browser -> protocol + core`, `cli -> core + transport`, `transport -> protocol`.
`core` and `protocol` must stay importable by both peers, so a Node import in either is a defect.

## Change rules

1. Keep the protocol strict. A frame change updates `types.ts`, `validate.ts`, both peers, fixtures and
   the protocol tests together. Limits are fixed constants, not negotiated values.
2. Never widen the extension permission surface. The manifest is exactly `activeTab`, `scripting`,
   `nativeMessaging`, and the packaging script fails the build if that changes or if ambient capture
   fields (`host_permissions`, `content_scripts`, `externally_connectable`) appear.
3. Authority comes from the browser, never from a message. Target identity (`tabId`, `documentId`,
   `pageEpoch`) is produced by the worker from Chrome APIs. A model- or page-supplied field is data,
   and any protocol field like `approved` or `grantId` inside an untrusted payload stays a schema
   error.
4. Treat page content as untrusted. It is never terminal text, never a command and never an implicit
   action. The CLI prints JSON records with control and bidi characters escaped.
5. Bound everything: bytes, nodes, characters, queue depth, identifiers, in-flight operations and
   lifetimes. Prefer fail-closed rejection to a fallback.
6. Keep the honest limits. Do not claim undetectability, native user activation, or that a dispatched
   page effect was undone. After a possible dispatch, an unconfirmed result is `unknown`.

## Adding a capability

The extension is intentionally narrow. A new capability means all of these, in this order:

1. Add the op and its result to `src/protocol/types.ts` plus validators and tests. Include the
   capability name in `CAPABILITIES` only if a user grant can enable it.
2. Implement the browser behavior in `browser/content/`, keeping the collector/action split: bounded
   projection in the collector, guarded mutation in `actions.ts`.
3. Enforce the grant in `browser/worker/`: capability check, binding match, observation freshness,
   active-tab rule, revocation on navigation/close/disconnect/expiry.
4. Expose it through the existing `BrowserPort` seam in `src/core` and use it from `src/cli`. A future
   model harness consumes the same port, so it must never pass grant fields taken from model output.
5. Add targeted tests. Then update this guide and the architecture document.

Add an interface when there is a caller, not in advance. Empty memory/vision/skills frameworks are
out of scope.

## Tests

Run only targeted suites, not the whole repository suite:

```bash
npm run check:navi
node --test packages/navi/test/protocol.test.ts
node --test packages/navi/test/core.test.ts
node --test packages/navi/test/transport.test.ts
node --test packages/navi/test/cli.test.ts
node --test packages/navi/test/security.test.ts
node --test packages/navi/browser/test/*.test.ts
```

A security-facing change needs an adversarial test that fails without the fix, not only a happy path.
Keep fixtures narrow: fake DOM nodes and a fake Chrome facade, with throwing getters where reading
`value`, `textContent` or `innerHTML` must be impossible.

Existing upstream checks stay untouched: `docs/navi-architecture/**` is an independent npm project
pinned to published Pi packages and must not be added to the root workspaces.

## Manual gates

State clearly in any handoff that these are not covered by automated tests:

- native host registration and the real caller origin Chrome passes to the connector;
- real popup versus content-script sender identity, including a late injection;
- navigation, BFCache, tab discard, tab replacement and worker suspension;
- real `focus`/`scroll` behavior on real sites, including site handlers.

Browser-behavior claims require running the browser, not a passing mock test.
