# Cross-review memo — NAVI bridge transport & page-processing split

Adjudicates positions 1–4 in `NAVI/research/`; repo `vendor/pi` @ `71dca871`, read-only, cited claims re-verified.

## Verdict

**(a) Transport — native messaging primary; loopback WebSocket the fallback.** Agent 3's socket-first ranking is wrong *as written* because its premise fails: **no browser extension, Chromium or Gecko, can open a UDS or named pipe.** The browser offers exactly two egress paths — a browser-spawned native-messaging host over stdio, or a WebSocket client to loopback. Agent 7's sharp formulation wins: the native-messaging host **is** the bridge, and its browser-managed lifetime yields lazy activation and teardown for free. Agent 3's UDS/named pipe is real but belongs to the *bridge↔NAVI-core* hop, not the browser hop. The WS fallback must carry Agent 6's controls: 127.0.0.1 bind only, per-launch 256-bit token, `Origin` check against the extension id, no CORS, fail closed.

**Which process the bridge is:** a standalone process, not an in-process Pi extension. The browser can only spawn a native host, so browser-initiated wake implies a separate process; and since UDS is unreachable, an in-process Pi-extension bridge still forces the browser onto a loopback *port* — reintroducing native messaging's removed hijack surface, and binding perception to a running Pi session. The bridge *may embed* Pi (Agent 10's shape); that is internal, not grounds to make the bridge a Pi extension.

**(b) Split.** Agents 3 and 10 agree: content script = DOM-only structural extraction plus first-pass redaction that never reads secret values; extension background = the only privileged component (policy, redaction re-check, batching, transport); bridge = framing, budgets, dedup, provenance/taint labelling, optional local summarisation; Pi = reasoning only. Agent 10 is right that semantic extraction is not the agent's job, but sharpen it: **structural** extraction needs the DOM → content script; **semantic summarisation** needs no DOM → bridge, never the least-trusted content script and never Pi. Hard line: **raw page text must never cross bridge→Pi**, nor enter Pi's system prompt, session file, or a provider request; it may cross content-script→background→bridge, bounded and labelled. Redaction is a pipeline: structural at capture (Agent 3), re-validated at background and bridge, egress-filtered last (Agent 6).

**(c) Protocol.** Envelope `{v, id?, ts, seq?, type, payload, trust?}`. Integer `protocolVersion` in the handshake *and* every frame (mirror `COORDINATOR_PROTOCOL_VERSION = 3`); unknown major → refuse and close, no half-compat; capability *sets* gate browser-specific features. `hello`/`hello_ack` first, carrying capabilities, `limits{maxFrameBytes, maxQueueFrames, maxQueueBytes}`, `sessionId`, and the WS token. Framing: use the browser's own length-prefixed frame on the native hop and one JSON object per WS frame; use LF-only JSONL **only** on stdio hops and never `readline`. MV3 SW death forces a **snapshot-authoritative** design: every SW start emits a fresh `state_snapshot`; deltas are advisory, carry `nav_seq`/`documentId`, and stale ones are dropped. Backpressure: bounded per-subscriber queue; overflow **coalesces** (collapse deltas by key, retain newest snapshot) and raises `resync_required`; never block; the extension rate-limits itself. Batching: 100–250 ms tick, immediate flush on user gesture and query answers. Liveness: `ping/pong` ~15 s, three misses → jittered reconnect. Idle exit: `unref()`'d timer, copying the coordinator's `EMPTY_SHUTDOWN_GRACE_MS`.

**Top failure modes.** (1) JSONL corruption from U+2028/U+2029 via `readline` → own the splitter; fuzz it. (2) SW-death desync → snapshot-authoritative resync. (3) Injection reaching Pi raw → the hard raw-text boundary, fenced/labelled blocks, `setActiveTools` allowlist; a `tool_call` gate is defence-in-depth, not the gate. (4) Port hijack on the fallback → Agent 6's controls. (5) Budget blow-up (a11y tree, screenshot) → per-signal caps at the bridge, blob spill frames, ≤1 screenshot/turn. (6) Blocked/frictional native messaging → WS behind one interface, per-OS host manifests plus a separate Gecko manifest.

## Evidence

- Extensions run in-process, unsandboxed, with Node built-ins and full permissions — `core/extensions/loader.ts:513`, `docs/security.md:31-35`. Supports "fewer processes", not "UDS".
- Framing hazard verified for stdio hops: `modes/rpc/jsonl.ts:14-20`, `docs/rpc.md:28-37`, `test/rpc-jsonl.test.ts` (U+2028/U+2029 survive inside a record). WS and native-messaging frames are discrete.
- Handshake/teardown to copy: `experimental/coordinator.ts:14` (version 3, register-then-ack, `unref()`'d empty-shutdown timer).
- Windows UDS gap: `packages/client/src/unix.ts:38,99` throw.
- Agent 3's 1 MB objection mis-aims: the ceiling lies on bridge→extension control frames, not the capture direction; it mandates spill frames, not abandonment. `[external — re-verify per browser]`

## Why the losing option remains attractive

The in-process Pi-extension bridge over UDS is *nearly* right: no new process, `ssh.ts` is a working template, `session_shutdown` gives teardown, and Pi ships `socketTransport` (`mini/shared/transport.ts`) to copy. UDS *is* the correct secure answer for two local processes — which is why the socket relocates to the bridge↔NAVI-core hop instead of being deleted.

## Conditions / triggers to switch

- **Native → WS-primary:** enterprise `NativeMessagingBlocklist`, unacceptable installer friction, a fork with no clean host path, or a non-browser client needing bridge access.
- **WS → native:** any port-hijack/DNS-rebinding finding, or a WS design that cannot hand its token to the extension without a world-readable file (native messaging conveys caller identity for free).
- **Bridge → shared daemon:** multiple profiles, or a tray/CLI/voice client; the native host hands off to a socket-activated daemon.
- **Standalone → in-process Pi extension:** only if NAVI is redefined as "perception live only while a Pi session runs", with the loopback port accepted or a relay present.
- **Summarisation:** never into Pi for untrusted text.

## Confidence

High: browser egress capability, the framing hazard, the Windows UDS gap, the bridge→Pi raw-text boundary. Medium-high: native-messaging-primary. Medium: per-browser message-size limits, per-transport SW-keepalive in current browsers (`[external]`), and Zen's native-messaging path (unverified). Residual risk is lifecycle, not transport — snapshot-authoritative resync must hold either way.
