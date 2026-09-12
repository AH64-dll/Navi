# Cross-review 1 — NAVI integration substrate (independent adjudication)

Verified in the read-only clone `C:\Users\lain\NAVI\vendor\pi` @ `71dca871` (v0.85.1). Agent 1 §8/A9, Agent 2 §2–§4, Agent 6 §B/E, Agent 8 §A3/R1 read first; every claim below re-checked in source.

## Verdict

**Position (1) wins, with one amendment borrowed from (2).** Primary = in-process SDK host: `createAgentSessionServices → createAgentSessionFromServices → createAgentSessionRuntime`, with NAVI supplying its own `ExtensionUIContext`, `SettingsManager` and `SessionManager`, and owning the fan-out hub (Agent 8 §R1). Secondary, not fallback-only: a pinned `pi --mode rpc` stdio child as a *second isolated runtime* for untrusted or long-running work, behind the same NAVI-internal port interface.

Agent 6's premise is sound but its conclusion does not follow: the only pre-execution block is the in-process `tool_call` handler (`runner.ts:982-1003`), so **policy always runs inside the Pi process regardless of transport**. RPC isolates NAVI's *host* from Pi crashes — not "permissions enforced outside Pi's message array". Real, but narrower than Position (3) claims.

**Migration path:** one NAVI-side `AgentPort` with two adapters (`SdkAgentPort` over `createAgentSessionRuntime`, `RpcAgentPort` over `RpcClient`), plus one versioned NAVI extension loaded identically via `extensionFactories` (SDK) and `-e <path>` (child). Gate/injection code is byte-identical; switching is a flag, not a rewrite.

## Evidence

- **SDK can render extension UI outside the TUI: TRUE.** `ExtensionUIContext` (`types.ts:133-284`) is exported from the package root (`index.ts:91`). NAVI binds its own via `session.bindExtensions({ uiContext, mode: "rpc" })` (`agent-session.ts:2468-2491`) → `runner.setUIContext` (`:2546-2547`), flipping `hasUI()` from `uiContext !== noOpUIContext` (`runner.ts:492-493`, set `:437`). `wrapUIPromptContext` also emits `ui_prompt_start/end` (`runner.ts:441-451`). TUI-typed members (`custom`, `setWidget(factory)`, `theme`) get stubs; the dialog subset suffices.
- **`ExtensionBindings` is NOT exported from `src/index.ts` — Agent 2 is correct** (no `export *` from `agent-session.ts`; `ExtensionUIContext` *is*, `index.ts:91`). Impact is small: structural typing accepts it. Brief's citation is off — `:232-239` *is* `ExtensionBindings`; `bindExtensions` is `:2468`.
- **RPC loses exactly what Agent 2 says.** `custom()` returns `undefined` (`rpc-mode.ts:228-231`; list at `docs/rpc.md:1195-1203`). The command union (`rpc-types.ts:20-74`) lacks per-tool invocation, tool introspection, `appendEntry`/custom-message injection, `importFromJsonl`, scoped models/settings, system prompt, or version field.
- **Trust asymmetry — the strongest pro-RPC evidence; neither report gets it right.** The SDK never calls `resolveProjectTrusted` (`agent-session-services.ts:147`, `sdk.ts:182`), and `SettingsManager.create(cwd, agentDir)` with no options defaults `projectTrusted` **true** (`settings-manager.ts:374`) — so project settings, `.pi` packages and project `SYSTEM.md` load *open* (`package-manager.ts:1742,2398`; `resource-loader.ts:1025,1039`). The CLI resolves trust explicitly (`main.ts:726-761`) and the RPC child inherits it. Forgetting the decision **fails open on SDK, fails closed on RPC**; one-line fix: `{ projectTrusted: false }` — which NAVI must own anyway (Agent 6 §E.2).
- **Other CLI-parity gaps:** `runMigrations(cwd)` runs only at `main.ts:649` and is **not exported from `index.ts`**; `ENV_SESSION_DIR` is honoured only by the CLI (`main.ts:671`), not by `getDefaultSessionDirPath` (`session-manager.ts:476-481`), so the SDK default (`sdk.ts:183`) writes into the user's real pi session store. Extension loading is *not* given up — `createAgentSessionServices` runs `resourceLoader.reload()` under `resourceLoaderOptions`/`extensionFactories` (`agent-session-services.ts:148-154`).
- **Upgrade resilience favours the SDK.** Its coupling is typed and fails at build time; RPC has no in-band version yet changed *semantics* within patches (`CHANGELOG.md:18,87`) — silent runtime skew. Loud > silent on a 9-minor/3-month cadence.
- **N surfaces, one session:** RPC has one stdout, no multi-attachment; NAVI owns the fan-out either way (Agent 8 §A3).
- **Windows:** both surfaces are stdio-only, hence Windows-safe; Pi's socket stack hard-throws there (`packages/client/src/unix.ts:38,99`), so NAVI's hub must avoid UDS.

## Why the losing option remains attractive

RPC gives crash isolation *pinned by test* (`test/rpc-client-process-exit.test.ts`) and lets the host restart while the session survives (`get_entries since`). It fails closed on trust by default. Its thin inbound surface is defensible for a permission-first product. And it keeps NAVI's volatile host code off the agent's thread, where a hung `tool_call` handler stalls the loop (awaited, serial, `runner.ts:851-883`).

## Conditions / triggers to switch to RPC-first

1. NAVI's host must survive its own crashes/restarts without losing the session (Windows service, plugin-hosted UI).
2. Measurement shows the SDK's eager module graph makes an always-on footprint unacceptable and a warm shared child wins.
3. The UI must be non-Node (Python/Rust/Go) — the SDK is then unavailable.
4. A tenant/threat model requires the agent in a separate OS process/uid.
5. Upstream adds an RPC version field + readiness frame + documented `appendEntry`/custom-message and tool-introspection commands — the control gap largely closes and isolation becomes near-free.
6. A NAVI gate handler is ever measured to stall the loop beyond the approval budget.

## Confidence

**Medium.** High on every factual claim above (re-read in source). Medium on the recommendation: the deciding input — cold-start and crash behaviour under an always-on ambient workload — is unmeasured, and the design rests on the untested assumption that NAVI's `tool_call` gate stays O(1) on the agent's thread. What would change it: (a) a measured `tool_call` latency budget, (b) `npm run profile:rpc` cold-start numbers, (c) upstream exporting `ExtensionBindings`/`runMigrations`, adding an RPC version field, or shipping a Windows transport for the experimental server.
