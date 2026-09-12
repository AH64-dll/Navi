# Architecture validation

These are executable reference contracts, **not a NAVI application**. They exercise a real published Pi SDK with a scripted provider, and deterministic protocol/policy cases without a browser.

```powershell
npm ci --ignore-scripts --no-audit --no-fund
npm test
```

Use Node 22.20.0 for the recorded environment. No model API key is needed. Dependency installation requires network access; the faux provider performs no inference network requests. Install scripts are disabled. The Windows dependency import is substantial; see the [measurement note](../research/11-implementation-review.md).

| File | What it establishes |
| --- | --- |
| [native-messaging.mjs](native-messaging.mjs) | 32-bit native-endian length, strict UTF-8 JSON objects, 256 KiB cap checked before allocation, partial-input handling |
| [native-messaging.test.mjs](native-messaging.test.mjs) | Every two-chunk split, one-byte chunks, coalesced frames, Arabic/emoji/Unicode separators, byte lengths, both byte orders, invalid/oversized/truncated input |
| [contracts.mjs](contracts.mjs) | Immutable binding comparison, independent page-byte cap, OBSERVE denies website execution, pre-call count/output/retry/cancel admission |
| [contracts.test.mjs](contracts.test.mjs) | Each binding component invalidates stale data; size/cancellation/annotation/call-limit failure cases |
| [pi-session.mjs](pi-session.mjs) | Published SDK imports, explicit in-memory configuration, minimal resource loader, custom-tool-only allowlist and stream admission wrapper |
| [pi-sdk.test.mjs](pi-sdk.test.mjs) | Real SDK/faux-provider round trip; neighboring AGENTS.md absent from context; no session/config files created in fixture directory; invented shell tool rejected; call/read/result limits; pre-cancelled request starts no provider call; host cancellation aborts a pending read |

`npm run test:contracts` runs without installing Pi. `npm run test:pi` runs just the SDK checks. Each SDK fixture uses a fresh temporary directory and removes only that verified fixture path after disposal. The SDK recipe budgets one Ask per constructed session; a production coordinator must renew per-Ask admission without resetting a live run or stacking stream wrappers.

These are regression checks for specific failure modes. They do not prove that arbitrary Pi dependencies perform no filesystem access: installed package files are necessarily read. They do not test real credentials, model accuracy, arbitrary provider retries/billing/tokenization, live provider cancellation, browser capture, worker suspension, sender authentication, schema validation, queue backpressure, native installation or process teardown. Those remain explicit gates in [the architecture](../NAVI-ARCHITECTURE.md).

The framing callback must be connected to a bounded production consumer. The scope checks assume the expected binding was established by trusted browser state; they do not authenticate it. `admitSiteTool` is a policy invariant, not a replay-safe approval implementation. The first release has no website-execution tool at all.
