# NAVI implementation review — 2026-09-13

This review supersedes conclusions in the original research where identified below. It supports [the current architecture](../NAVI-ARCHITECTURE.md). “Verified” below means a specific source or executable check was inspected; it does not mean the proposed application already works.

## Corrections that affect implementation

| Earlier assumption | Finding | Applied decision |
| --- | --- | --- |
| JSONL on the native-messaging hop | Browser native messaging has a binary length header. XR3 had this right, but the synthesis contradicted it. | Length-prefixed UTF-8, with fragmented/coalesced/oversized-frame regression checks |
| Browser-native host plus a separately spawned core | Unnecessary process boundary for the initial workload | The host is the coordinator/core |
| Parked worker while keeping the native connection | A live native port keeps MV3 alive | Close after a bounded grace; no automatic reconnect loop |
| `activeTab` supports ambient observation | It is a temporary gesture grant, with navigation-specific lifetime | Deliberate capture; per-origin background access requires a later permission flow |
| Browsers supply extraction semantics to extensions automatically | DOM projection and exclusion traversal must be implemented; AXTree/readability are not free extension APIs | Explicit extractor, work limits and partial-coverage behavior |
| Default loader plus `projectTrusted:false` suppresses local discovery | Global configuration/package discovery remains possible | Minimal custom ResourceLoader plus in-memory settings/credentials/session |
| `customPrompt` is a session-factory option | It is absent from the pinned API | Persona comes from ResourceLoader |
| `tools:[]` plus customTools exposes custom tools | Explicit allowlist applies to custom tools too | `noTools:'all'`, `tools:['navi_page']`, and the matching custom tool |
| Budgeting from emitted assistant events stops the next request | `message_start` follows provider startup | Reserve before invoking the public streamFunction; disable both retry layers |
| No context handler avoids the clone | The earlier cross-review already showed the clone happens before checking handlers | Keep sessions short; do not claim this configuration removes the allocation |
| Unchanged hash means zero model tokens; cache always lasts five minutes | Transfer deduplication is not inference accounting, and cache behavior varies | Separate byte limits, tokenizer accounting, provider usage and cache measurements |
| Restoring page state supplies undo | Server-side effects may be irreversible | Postconditions and explicit compensation/retry rules for any later action |
| A separate RPC process isolates untrusted code securely | It remains under the same account without a sandbox | No security-isolation claim; omit the unused second runtime |
| Memory, ambient suggestions and source research are all first-release foundations | They enlarge capture and lifecycle scope before value is established | Page/selection answers first; enhancements have independent task gates |

Browser source basis: [native messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging), [activeTab](https://developer.chrome.com/docs/extensions/develop/concepts/activeTab), [worker lifecycle](https://developer.chrome.com/docs/extensions/develop/concepts/service-workers/lifecycle), accessed 2026-09-13.

Pi source basis at `71dca871bc80b6bc97be37f0ca3189399d651fff`:

- `packages/coding-agent/src/core/sdk.ts:39` — factory options and explicit tool configuration.
- `packages/coding-agent/src/core/resource-loader.ts:388` and `package-manager.ts:912` — default discovery remains active at package resolution.
- `packages/agent/src/agent-loop.ts:293` — stream function is called before assistant stream events are consumed.
- `packages/coding-agent/src/core/agent-session.ts:1101` — outer retry; `:1917` — compaction uses the same stream function; `:2146` — disabled-compaction guard.
- `packages/coding-agent/src/core/model-runtime.ts:636` and `packages/ai/src/api/lazy.ts:46` — provider setup at stream creation.

These were inspected in the existing read-only vendor clone by the Pi audit. The published-package checks independently exercise the proposed public imports and behavior; they do not rely on `vendor/` being present.

## Sodium: actual role and compatibility

The [Sodium site](https://sodium.result.dev/) describes website-authored `sodium.json` tools, deployment and a browser SDK. Its role is to publish a site's explicit capabilities. NAVI is on the consuming side. Installing the SDK into NAVI does not grant typed access to arbitrary websites.

Registry/artifact evidence inspected without running Sodium init/deploy or package installation scripts:

| Artifact | Exact version | Evidence |
| --- | --- | --- |
| CLI | `sodiumtools@0.3.0` | [Registry metadata](https://registry.npmjs.org/sodiumtools/0.3.0); tarball `dist/bin.js` installs `sodium-webmcp-sdk` in the website project |
| Website SDK | `sodium-webmcp-sdk@0.2.0` | [Registry metadata](https://registry.npmjs.org/sodium-webmcp-sdk/0.2.0); `dist/types/sdk.d.ts`, `webmcp-adapter.d.ts`, `types.d.ts`, `dist/index.js` |
| Declaration dependency | `sodium-webmcp-spec@0.2.0` | [Registry metadata](https://registry.npmjs.org/sodium-webmcp-spec/0.2.0); exact SDK dependency, rather than a NAVI runtime dependency |

Downloaded tarball SHA-256:

```text
sodiumtools-0.3.0.tgz
09e8d0eb86dd4972a861586ae4be7e2abf503451967df5853c22fbea3ffea6ed
sodium-webmcp-sdk-0.2.0.tgz
90995d146490cbcc038323f4f1f0697401e672a1f65b413e8fed9d8779819a21
```

Observed SDK behavior:

- `installSodium` validates deployment receipt/config/origin before registration; missing or mismatched deployment disables the tools.
- `detectModelContext` uses `document.modelContext` and then a deprecated `navigator.modelContext` fallback. It tests producer registration, which alone does not establish consumer access.
- Registration projects `readOnlyHint` and `untrustedContentHint`. The SDK's richer internal risk/confirmation values are not all exposed as structured WebMCP fields; some become description text. NAVI must not assume it receives a trusted `risk` enum or signed receipt through generic discovery.
- The published handler code handles required confirmation, while recommended confirmation can be descriptive advice. NAVI still owns trusted approval.
- Telemetry defaults enabled in compiled configuration; the SDK may send site events to its configured endpoint. NAVI consumption does not require adding Sodium telemetry to the extension, but it cannot promise that the website emits none.

These findings support an optional consumer, not copying the SDK into the browser extension. No Sodium runtime dependency was added to this repository.

The [WebMCP draft](https://webmachinelearning.github.io/webmcp/) inspected is dated 2026-09-10 and explicitly is not a W3C Standard. It describes `document.modelContext.getTools`/`executeTool` and browser-realm tool objects. [Chrome's guide](https://developer.chrome.com/docs/ai/webmcp) documents an origin trial starting at Chrome 149 and a local flag; [Chrome security guidance](https://developer.chrome.com/docs/ai/webmcp/secure-tools) documents content-script access and annotation hints. Actual extension/browser compatibility has **not** been tested here. The feature-detected pilot and ordinary DOM fallback are therefore required, not optional polish.

## Executable results and limits

Environment: Windows, Node `v22.20.0`, npm `10.9.3`. Exact Pi packages `0.85.1` were installed with `--ignore-scripts`; the repository now contains a reproducible dependency lock.

After a clean locked installation, **all 17 tests passed**: 10 framing/policy tests and 7 published-SDK tests. SDK checks cover a page-tool round trip, rejection before an excess provider call, unavailable shell tool, pre-cancelled request, read-count limit, oversized result exclusion and cancellation during a pending page read. Evidence is scripted, without a real browser or live inference. See [validation coverage](../validation/README.md).

**Performance finding:** the first complete SDK test process took 23.48 seconds, while its four test bodies totalled about 236 ms. Two separate fresh-process imports of the published coding-agent root took **17,094 ms** and **6,226 ms**. These are local samples, not a percentile benchmark; module import is separate from `createAgentSession` timing. The public root exports the CLI, run modes and UI components as well as the SDK, so the earlier “no TUI imports” entry-graph assumption does not hold for an ordinary root import. Do not infer the exact cause of elapsed time from that graph alone.

This fails the proposed two-second local startup target in the measured environment. SDK construction is demonstrated; acceptable cold startup is **not**. Stage B must measure packaged startup and try bundling the public entry with unused exports removed, then run these same contract checks on that artifact. Do not rely on unexported deep imports. If startup still fails the product gate, use the direct-call adapter or an explicitly chosen bounded warm policy after measuring its cost. Do not silently restore a permanent daemon or declare the latency solved.

Unverified release work: actual extension extraction/redaction, native launcher/registration and binary pipes, sender/approval authentication, live navigation races, worker restart, backpressure/child cleanup, Linux behavior, tokenization/provider usage, live cancellation, answer quality, Sodium browser consumption and useful real-site coverage.
