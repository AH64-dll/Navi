# NAVI — Performance & Cross-Platform Architecture (Agent 7 of 10)

**Scope:** design research only. No implementation, no repo writes.
**Evidence base:** read-only clone at `C:\Users\lain\NAVI\vendor\pi` (`github.com/earendil-works/pi`, branch `main`, commit `71dca871`, 2026-09-11; `@earendil-works/pi-coding-agent v0.85.1`).
**Citation convention:** `path:line → symbol → implication`. Tags: `[verified]` (read in source), `[inferred]` (reasoned from source), `[external]` (general platform knowledge, not from this repo).

> **Measurement status.** Nothing in this report was benchmarked against a running organism. The repo was not built or executed (builds/installs are disallowed for this task). Every numeric value is either (a) a literal constant read out of Pi source, cited inline, or (b) a **design target / estimate**, labelled as such, with the reasoning given. Numbers marked `TARGET` must be validated by a future prototype using the "how measured" column; they are not measurements.

---

## Repository Findings

### 1. Startup path: eager module graph, three lazy islands

**Entry chain.** `src/cli.ts:1-6 → setupCli() → main(process.argv.slice(2))` is a two-line shim; `setupCli` and `main` are *static* imports, so the whole module graph is resolved before any work begins. `[verified]`

**`main.ts` is fully eager.** `packages/coding-agent/src/main.ts:8-71` statically imports ~60 modules straight into the top level, including:
- terminal/UI surfaces: `InteractiveMode`, `runPrintMode`, `runRpcMode` from `./modes/index.ts` (`main.ts:66`), theme subsystem (`main.ts:67`), export-html (`main.ts:45`);
- heavy core: `SessionManager` (`:59`), `SettingsManager` (`:61`), `AuthStorage` (`:44`), `ModelRuntime` (`:49`), `createAgentSessionServices` (`:38-42`), migrations (`:65`), `package-manager-cli` (`:69`).

There is **no dynamic `import()` anywhere in `main.ts`** `[verified]`. Consequently `pi --mode rpc` and `pi --print` still evaluate the InteractiveMode/TUI graph, themes, the model registry, and auth storage before the mode switch at `main.ts:930-980`. Implication: **Pi's per-mode cost is not mode-optimised at the module level**; the only thing that changes is which `run*` function is finally called. NAVI must not copy this — the RPC child should not pay for TUI/theme/export code.

**Lazy islands (the only three in coding-agent).** `[verified]`
| What | Where | Trigger |
|---|---|---|
| Photon image/WASM (`@silvia-odwyer/photon-node`) | `src/utils/photon.ts:32-33,116-128` (`loadPhoton`) | first image read/convert |
| syntax highlighting (`highlight.js`) | `src/utils/syntax-highlight.ts:57` (`void import("highlight.js/lib/index.js")`) | first code block rendered |
| extension loading (`jiti`) | `src/core/extensions/loader.ts:513` (`await jiti.import(extensionPath, { default: true })`) | first user extension |

The bundler preserves three of these as deliberate `external` + deferred loads: `packages/coding-agent/package.json` deps list `jiti`, `@silvia-odwyer/photon-node`; `scripts/build-coding-agent-bundle.mjs:18-33` (`allowedExternalPackages`) keeps `jiti`, `photon-node`, `bufferutil`, `utf-8-validate`, `kerberos`, `supports-color` external, and `:35-57` (`lazyJitiPlugin`) rewrites `jiti/static` into a synchronous lazy `require` so Babel only loads on a cache miss. `[verified]`

**Bundle splitting.** `scripts/build-coding-agent-bundle.mjs:161-172` builds `cli`, `index`, `rpc-entry` with `splitting: true` and `chunkNames: "chunks/[name]-[hash]"`; `:184-200` emits standalone lazy files for OAuth providers (anthropic, github-copilot, kimi-coding, openai-codex, openrouter, radius, xai), bedrock, and the image-resize worker, because they are reached by variable specifiers or a worker URL. `[verified]` Implication: **Pi already ships a split-chunk design**; NAVI can legitimately do the same (separate chunk for the indexer, for TUI, for each provider) instead of shipping a monolith.

**Barrel hazard, quantified by the repo itself.** `packages/coding-agent/src/index.ts` is a 425-line re-export barrel over the entire SDK. `scripts/check-entry-graphs.mjs:2-12` states in a comment that *"importing a 1-file pure function through a barrel costs ~37 MB of evaluated module graph"* and enforces per-entry budgets (`maxFiles`, `forbid`) in CI via `npm run check:entry-graphs`. `[verified]` Implication: **module-graph size is treated as a first-class cost contract** — NAVI should adopt the same budget-check idea for its own entry points.

### 2. Native modules (`packages/tui/src` + `packages/tui/native`)

**Resolution.** `native-module-path.ts:14-31 → getNativeModuleCandidates` tries, in order: the installed `@earendil-works/pi-tui` package dir, `../<nativePath>`, `<nativePath>` relative to the module, and `<dirname(process.execPath)>/<nativePath>`; the first failure path is explicitly for standalone binaries ("Standalone binaries do not have an installed TUI package"). `[verified]`

**Loading policy.** `native-platform.ts:23-56`:
- caches by *module path*, deliberately **not** by display availability ("a disconnected display can recover", `:23`);
- returns `undefined` for arch ∉ {x64, arm64} (`:28`);
- `getNativePlatformHelper()` returns undefined unless `darwin`/`win32` (`:54`);
- `getNativeClipboard()` returns undefined on Linux unless `process.env.DISPLAY` is set, and then loads a `-x11` variant (`:59-62`). `[verified]`
Implication: **the native helper is a lazily `require`d, optional accelerator with a JS fallback**, gated on platform+arch+display. On a pure-Wayland Linux session (`DISPLAY` unset) Pi has **no native clipboard at all** — NAVI must assume that too. `[verified]`

**What is compiled, per platform:**
| Platform | Source | Toolchain / flags | Purpose |
|---|---|---|---|
| win32 x64+arm64 | `native/win32/src/win32-platform.c` | MSVC (`cl /O1 /GS- /LD /NOENTRY /NODEFAULTLIB kernel32.lib user32.lib /OPT:REF /OPT:ICF`, `native/win32/build.mjs:130-167`) or MinGW/clang (`-Oz -nostdlib -Wl,--no-entry --strip-all`, `:169-201`) | clipboard get/set + `enableVirtualTerminalInput` + `isModifierPressed` (`native-platform.ts:18-21`) |
| linux x64+arm64 | `native/linux/src/linux-platform-x11.c` | `cc -Os -flto -nostdlib -fvisibility=hidden -shared -lxcb -Wl,-z,nodelete -Wl,-s` (`native/linux/build.sh:33-64`) | X11 clipboard; comment at `:51` — "A timed-out private thread may outlive the Node environment that loaded us" → `nodelete` |
| darwin arm64+x64 | `native/darwin/src/darwin-platform.m` | `clang -fobjc-arc -Oz -flto -bundle -framework AppKit/CoreGraphics/Foundation`, targets `arm64-apple-macos11.0` / `x86_64-apple-macos10.15` (`native/darwin/build.sh:43-69`) | AppKit clipboard + modifier keys |

`[verified]` **Build is a maintainer step, not part of the app build:** root `package.json:18-20` exposes `build:native:darwin|linux|win32`, but `npm run build` (`package.json:16`) does **not** invoke them; the compiled `.node` files are committed/shipped (`packages/tui/package.json:16-33` `files` includes `native/*/prebuilds/**/*.node` and the C sources + build scripts). `[verified]` Implication: **native addons are prebuilt and shipped per-arch, with sources included for auditability.** NAVI should do the same (and treat cross-arch as a build-matrix problem, not a runtime problem).

### 3. Platform-sensitive code paths

- **Process spawning.** `utils/child-process.ts:24-36 → spawnProcess/spawnProcessSync` uses `cross-spawn` **only on win32**, plain `node:child_process` elsewhere, because of Windows `.cmd` shims (`CHANGELOG.md:1472` cites issue #4665). `[verified]`
- **Detach semantics.** `core/tools/bash.ts:94-100` spawns with `detached: process.platform !== "win32"` and `windowsHide: true` — Windows cannot detach the same way, so it never gets its own process group. `[verified]`
- **The Windows stdio hang.** `utils/child-process.ts:38-137 → waitForChildProcess` exists specifically because a short-lived shell can `exit` while a detached descendant keeps the stdout/stderr pipe open; it waits for pipes to go idle with a **100 ms grace that re-arms on every chunk** (`EXIT_STDIO_GRACE_MS = 100`, `:16`, `:88-97`). Comment references `earendil-works/pi#5303`. `[verified]`
- **Tree kill.** `utils/shell.ts:216-247 → killProcessTree`: win32 → `taskkill.exe /F /T /PID` invoked from the **trusted** `%SystemRoot%\System32` path (`:221`); POSIX → `process.kill(-pid, SIGKILL)` then single-pid fallback. A set of tracked detached PIDs is killed on shutdown (`utils/shell.ts:196-211`). `[verified]`
- **Shell discovery.** `utils/shell.ts:67-120 → getShellConfig`: win32 → Git Bash under `%ProgramFiles%`/`%ProgramFiles(x86)%`, then `where bash.exe`; POSIX → `/bin/bash` → `which bash` → `sh`. Discovery uses `spawnSync("where"|"which")` with 5000 ms timeouts (`utils/shell.ts:24-58`) — i.e. **a synchronous subprocess spawn on the shell-tool path**, which NAVI should not replicate hot. `[verified]`
- **PowerShell tool.** `core/tools/powershell.ts:32-37` + `utils/shell.ts:122-136`: win32-only, prefers `pwsh.exe` over `powershell.exe`, args `-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command`, and prepends `[Console]::OutputEncoding=UTF8` (`powershell.ts:16`). `[verified]`
- **Path handling.** `utils/paths.ts:66-100 → normalizeWindowsShellPath/normalizePath` converts POSIX-style `/mnt/c/…`, `/cygdrive/c/…` to `C:\…` and special-cases `~\` on win32; `resolvePath` (`:102-106`) uses `node:path` so it is correct per-platform. `[verified]`
- **Signals.** `modes/rpc/rpc-mode.ts:366-380` registers `SIGTERM` everywhere but `SIGHUP` **only when `process.platform !== "win32"`**; RPC mode has no other liveness mechanism. `[verified]`
- **Interactive suspend.** `modes/interactive/interactive-mode.ts:4088-4123 → handleCtrlZ`: on win32 it just prints "Suspend to background is not supported on Windows" (`:4089-4092`); POSIX uses `SIGTSTP`/`SIGCONT` plus a keep-alive `setInterval(() => {}, 2 ** 30)` (`:4097`) to stop Node exiting with no ref'd handles. `[verified]`
- **Windows-only startup work.** `main.ts:575-577` → `cleanupWindowsSelfUpdateQuarantine(...)`; `main.ts:588-594` deliberately **skips `process.exit(0)`** after a successful `pi update` on Windows to dodge a Node teardown assertion (nodejs/node#56645). `[verified]`

### 4. Tests that encode platform behavior

| Test | Gate | What it pins |
|---|---|---|
| `test/bash-close-hang-windows.test.ts:72` | `describe.skipIf(platform !== "win32")` | executor + bash tool resolve after the shell exits even when a detached descendant holds inherited stdio; cleans up with `taskkill /F /T` (`:36`) |
| `test/powershell-tool.test.ts:13-29` | assertion always; UTF-8 round-trip `skipIf(platform !== "win32")` | `POWERSHELL_ARGS` exact array; `pwsh` UTF-8 output + `Bypass` policy |
| `packages/agent/test/harness/nodejs-env.test.ts:441,523,581` | `skipIf(win32)` / `skipIf(not win32)` | Windows signal→exit-code mapping; "ignores asynchronous taskkill spawn errors during abort" |
| `test/paths.test.ts:110-154`, `test/config.test.ts:126-142`, `test/package-manager.test.ts:516,1676` | win32 branches | drive-letter normalisation; `.cmd` shims; `junction` vs `dir` symlink type |
| `packages/coding-agent/test/experimental-internal-process.test.ts:26`, `packages/client/test/unix-transport.test.ts:54`, `packages/server/test/unix.test.ts:102` | `skipIf(win32)` | the entire UDS/server stack is POSIX-only |
| POSIX-only mode tests | `skipIf(win32)` | `auth-storage` file modes, `models-store` mode preservation |

`[verified]` Pattern: **Windows-specific tests exist but are narrow** (process-close hangs, PowerShell, `.cmd`, paths); the whole socket/server layer is explicitly non-Windows (`packages/client/src/unix.ts:38,99` throw `"Unix transport is not supported on Windows"`).

### 5. CI: which OSes are actually tested

- `.github/workflows/ci.yml:14-42`: a **single** job `build-check-test` on **`ubuntu-latest` only**, Node 22, `npm ci --ignore-scripts`, `npm run build`, `npm run check`, `npm test`. `[verified]`
- `.github/workflows/build-binaries.yml:131-200`: `smoke-test-binaries` runs on `ubuntu-latest`, `macos-latest`, **`windows-latest`**, but only executes `"$binary" --help` and `--version` (`:196-200`). Binaries are built once on ubuntu with Bun for 6 targets (`:93-100`). `[verified]`

**Implication (important risk line for NAVI):** Pi's full test suite **never runs on Windows in CI**; Windows gets only a launch smoke test. Everything Windows-correct in Pi relies on hand-written guarded tests that a Linux CI cannot execute. NAVI must not assume Pi's Windows behaviour is regression-protected — it is not. `[verified]`

### 6. Performance tooling already in the repo

- **Startup profiler** `scripts/profile-coding-agent-node.mjs`: `--mode tui|rpc`, `--runs`, `--warmup`, `--cpu-profile`, `--bundle`, `--runtime node|bun`, `--isolated-agent-dir`, `--skip-build`. RPC mode writes `{"id":…,"type":"get_state"}` and times the response (`:479-522`); TUI mode sets `PI_STARTUP_BENCHMARK=1` (`:390-392`). Emits machine-readable `METRIC startup_time_ms=…` and `METRIC <label>_ms=…` lines (`:645-648`, `:666-669`). Exposed as `npm run profile:tui` / `profile:rpc` (`package.json:37-38`). `[verified]`
- **Phase timings** `core/timings.ts:6,16-50`: gated by `PI_TIMING=1`; labels are placed through `main.ts` at `parseArgs` (`:613`), `runMigrations` (`:650`), `firstTimeSetup` (`:659`), `createSessionManager` (`:698`), `createRuntime` (`:840`), `createAgentSessionRuntime` (`:846`), `readPipedStdin` (`:877`), `prepareInitialMessage` (`:884`), `initTheme` (`:888`), `resolveModelScope` (`:895`), `createAgentSession` (`:907`), `interactiveMode.init` (`:948`). `[verified]`
- **Cache accounting** `core/cache-stats.ts:8,11,138-163`: `CACHE_TTL_MS = 5*60*1000` (Anthropic default), `NOISE_FLOOR_TOKENS = 1024`, and `computeCacheWaste` / `detectCacheMiss` that compute *dollars wasted* on prompt-cache misses from session entries. `[verified]` Implication: Pi already treats **prompt-cache stability as a measurable cost**; NAVI should reuse this logic and treat context mutation as a spend event.
- **Telemetry** `packages/telemetry/src/index.ts`: default export is `NOOP_TELEMETRY_CONTEXT` (`:24`), plus an `InMemoryTelemetryContext` for tests. Startup network work is all background and gated: version check (`utils/version-check.ts:97-98`, returns early on `PI_SKIP_VERSION_CHECK`), install report + package update check (`modes/interactive/interactive-mode.ts:1046-1066`), remote model catalog refresh (`core/remote-catalog-provider.ts:8`, `REMOTE_CATALOG_REFRESH_INTERVAL_MS = 4h`). `[verified]`
- **HTTP** `core/http-dispatcher.ts:4,81-113`: undici `EnvHttpProxyAgent`, `DEFAULT_HTTP_IDLE_TIMEOUT_MS = 300_000`, connection pool **kept alive** by default (adjustable 30 s…disabled). `[verified]` Implication: an idle Pi process still holds keep-alive sockets for up to 5 min unless reconfigured.

### 7. Idle-runtime behaviour: where the cost actually sits

- **RPC mode never exits on its own.** `modes/rpc/rpc-mode.ts:819-820` ends with `// Keep process alive forever` → `return new Promise(() => {})`. Shutdown requires stdin `end` (`:804-807`) or SIGTERM/SIGHUP (`:728-745`). `[verified]` This is the single most important fact for NAVI: **Pi's RPC child is a permanent process by design; idleness management must live in the parent.**
- **Render loop is coalesced and demand-driven.** `packages/tui/src/tui.ts:477` `MIN_RENDER_INTERVAL_MS = 16` (~60 fps cap); `:986-1004 → scheduleRender` computes `delay = max(0, 16 - elapsed)`; `:952-961 → requestRender` coalesces via `process.nextTick`; `:963-978,1078-1081 → requestImmediateRender` lets keystrokes preempt the throttle ("even `setTimeout(0)` can take a full 16 ms tick on Windows"). `[verified]` There is **no idle frame loop** — rendering stops when nothing calls `requestRender`.
- **Residual timers while interactive.** `[verified]`
  | Timer | Rate | Where |
  |---|---|---|
  | git HEAD `fs.watch` debounce | 500 ms (`WATCH_DEBOUNCE_MS`) | `core/footer-data-provider.ts:101,210` |
  | git HEAD `watchFile` **polling** (conditional) | 1000 ms | `core/footer-data-provider.ts:336` |
  | reftable `tables.list` **polling** | 250 ms | `core/footer-data-provider.ts:370` |
  | theme `fs.watch` | event-driven, interactive only | `modes/interactive/theme/theme.ts:759,779-781` |
  | spinner | 80 ms, only while a loader is shown | `packages/tui/src/components/loader.ts:12,82-85` |
  | bash output flush | 100 ms throttle (`BASH_UPDATE_THROTTLE_MS`) | `core/tools/renderers/bash.ts:19`, `core/tools/bash.ts:281-294` |
  | terminal progress keep-alive | 1000 ms, only while progress is active | `packages/tui/src/terminal.ts:8,530` |
  | alt-screen selection auto-scroll | 50 ms, only while dragging | `packages/tui/src/tui-alt-screen.ts:1270` |

  Implication: **Pi's idle CPU is ~0 except for the conditional `watchFile` polling**, which is precisely the anti-pattern NAVI must avoid. `watchFile` is used because `fs.watch` on a *file* breaks when git replaces it via rename (inode change) — `core/footer-data-provider.ts:313-315`. `[verified]`
- **Transient workers are created and destroyed per use.** `utils/image-resize.ts:21-76`: `new Worker(...)` per resize, `worker.terminate()` in a `finally` (`:73-75`), with an in-process fallback (`:105-109`). `[verified]` — good precedent: heavy native/WASM work is off-thread **and** not persistent.

### 8. Pi's own daemon/worker precedent (the closest thing to NAVI's design)

The `experimental` "remote harness" in `packages/coding-agent/src/experimental/` is a working model of exactly NAVI's problem — a long-lived local service with on-demand children:

- **Roles + spawn.** `experimental/process.ts:41-70 → spawnInternalProcess(role, args)`, roles `"coordinator" | "server" | "session-worker"` (`:8`), spawned **detached, `stdio: "ignore"`, `windowsHide: true`, `child.unref()`**, tagged via env `__PI_INTERNAL_SPAWN` (`:6,62`). `[verified]`
- **Lazy bring-up.** `experimental/coordinator.ts:187-199 → ensureCoordinator`: *try to connect to the socket; only spawn a coordinator if the connect fails*, then poll-connect every **10 ms** until a **10 s** deadline (`COORDINATOR_RETRY_MS=10`, `COORDINATOR_START_TIMEOUT_MS=10_000`, `:15-16`). `[verified]` This is textbook lazy activation.
- **Idle self-shutdown.** `experimental/coordinator.ts:263-264, 483-506`: `EMPTY_STARTUP_GRACE_MS = 30_000`, `EMPTY_SHUTDOWN_GRACE_MS = 250`; `checkEmpty()` schedules `shutdownCoordinator()` only when there is no server, no peers and no connections, using an **unref'd** timer (`:499`). `[verified]` **This is the single best architectural precedent in the repo for NAVI's idle-exit requirement.**
- **Transport shim.** The coordinator is deliberately "a transport shim … depends only on Node built-ins and never interprets Pi, session, worker, or lifecycle payloads" (`coordinator.ts:260-261`), routing JSON lines (`encodeControlLine`, `MAX_CONTROL_LINE_BYTES = 128 MiB`, `process.ts:99-104`). `[verified]`
- **Unix-domain sockets, POSIX-only.** `packages/server/src/transports/unix/listener.ts:47-86`: `net.createServer` bound to a UDS path, mode `0o600` (`:11`), stale-socket probe, bind-to-temp-path then atomic `link()` (`:72-79`), graceful-close timeout 5 s (`:12`). `packages/client/src/unix.ts:38,99` throw on Windows. Socket path = `<serverDir>/<uuidv4>.sock` (`address.ts:4-8`). `[verified]`
- **Worker lifecycle timeouts.** `experimental/session-worker-manager.ts:34-37`: startup 15 s, shutdown 10 s, discovery 5 s, demand 5 s. `[verified]`

Implication for NAVI: **the "persistent coordinator + lazily spawned workers + idle self-exit" pattern is already validated inside Pi**, including the awkward parts (connect-or-spawn race, stale socket detection, owner-only perms). NAVI should reuse the *pattern* (and ideally Pi's `pi-server`/`pi-client` code, which is POSIX-only today) rather than invent one.

### 9. RPC as NAVI's Pi integration surface

- **Server side:** `modes/rpc/rpc-mode.ts` — strict LF-only JSONL over stdin/stdout; commands in, responses + `AgentSessionEvent`s out; `get_state` (`:450-466`), `prompt` (`:394-416`), `bash` (`:563-584`), `compact`, `new_session`, `switch_session`, `fork`, etc.; extension-UI round-trips via `extension_ui_request`/`extension_ui_response` (`:769-782`). Frames written with backpressure: every response awaits `waitForRawStdoutBackpressure()` (`:361-363, 789`). `[verified]`
- **Framing:** `modes/rpc/jsonl.ts:10-12,21-41` — `JSON.stringify(v) + "\n"`, reader splits on `\n` only, explicitly **not** `readline` (which splits on U+2028/U+2029 inside JSON strings). `[verified]` NAVI should reuse this exact framing contract.
- **Stdout hijack:** `core/output-guard.ts:45-70 → takeOverStdout()` redirects `process.stdout.write` to stderr so accidental `console.log` cannot corrupt the protocol; `writeRawStdout` serialises through a promise tail (`:85-93`) and retries `ENOBUFS`/`EAGAIN`/`EWOULDBLOCK` at 10 ms (`:9,34-42`). `[verified]` This is a reusable pattern for any NAVI IPC channel that shares stdout.
- **Client side:** `modes/rpc/rpc-client.ts:56-140 → RpcClient` spawns `node dist/cli.js --mode rpc` with `stdio: ["pipe","pipe","pipe"]` (`:94-98`), then **blindly waits 100 ms** before checking liveness (`:132-139`). `[verified]` Implication: **the repo's own client has no readiness handshake** — it sleeps. NAVI should do better (announce-ready frame, à la `get_state` in the profiler) and should own the child's idle timeout.

---

## Architectural Implications

1. **Eager startup is the default failure mode, and Pi shows it.** `main.ts`'s static graph means mode selection saves nothing at import time. A browser-ambient harness that must "stay lightweight while the user browses" cannot afford to evaluate a TUI + theme + model-registry + auth graph on a cold path. **Decision: NAVI's components are separate processes with separate entry graphs, budget-checked like `check-entry-graphs.mjs`.** `[verified]`→`[inferred]`
2. **Nothing in Pi gives away idleness for free — except the coordinator.** The RPC child is *designed to stay alive forever* (`rpc-mode.ts:819-820`). The only self-terminating service in the repo is the experimental coordinator, and it does it with an unref'd "empty" timer. **Decision: NAVI owns idle lifecycle; the Pi child is disposable.** `[verified]`
3. **`fs.watch` vs polling is a real cross-platform fork.** Pi polls git HEAD every 1 s to survive rename-based atomic writes. NAVI's dominant watch target is the *browser*, not git, so polling should be replaced by event streams (extension → bridge) and native recursive watching where the OS supports it. `[verified]`→`[inferred]`
4. **The browser hop constrains IPC choices absolutely.** A Chromium/Firefox extension cannot open a UDS or a Windows named pipe. It can use (a) **native messaging** (the browser spawns a host and speaks length-prefixed JSON over stdio) or (b) a **localhost WebSocket**. So the "IPC choice" question splits into two hops with different answers. `[external]` (platform capability), consistent with `packages/client/src/unix.ts` being browser-unreachable. `[inferred]`
5. **Windows correctness in Pi is not regression-protected.** Full suite is ubuntu-only. NAVI must budget its own Windows CI (at minimum: unit tests + a bridge/pipes integration test on `windows-latest`). `[verified]`
6. **Prebuilt, per-arch native helpers with JS fallbacks** is the established pattern and the right one for NAVI's optional accelerators (clipboard, modifier keys, maybe a file-walker). `[verified]`
7. **Prompt-cache stability is a cost lever, not a nicety.** `cache-stats.ts` computes re-billed tokens and dollars. Any NAVI feature that injects volatile context (timestamps, page fragments, live DOM) at the *front* of the prompt destroys the cache and multiplies spend. `[verified]`→`[inferred]`
8. **Backpressure must be first-class.** Every IPC hop in Pi has an explicit write-tail + backpressure wait. A browser-event firehose into a queue with no backpressure is the most likely NAVI failure mode (unbounded memory). `[verified]`→`[inferred]`

---

## Recommended Approach

### R0. Component model: three processes, one of them optional

```
Browser extension  ──(native messaging stdio JSONL, or WS fallback)──▶  navi-bridge (daemon, idle-exits)
                                                                              │
                                                       (UDS / named pipe, or stdio JSONL)
                                                                              ▼
                                                                     pi --mode rpc  (child, idle-exits)
                                                                              ▲
                                                                     navi-indexer (child, on-demand, exits)
```

| Component | Lifetime | Why |
|---|---|---|
| **Browser extension** | Browser-managed (MV3 service worker is *already* event-driven and evicted when idle) | zero standing cost; browser did the work for us |
| **`navi-bridge`** | Persistent **only while a browser session is active**; self-exits after idle timeout, restarts on first event (or on socket connect via socket activation) | single owner of the Pi child, the queue, and the indexer schedule |
| **`pi --mode rpc` child** | Spawned on first LLM/intent need; killed by bridge after idle timeout | Pi's RPC mode never self-exits → bridge must own its lifetime |
| **`navi-indexer`** | On-demand child; runs to completion or yields; **never resident** | bulk work, no interactive latency requirement |

Rationale for a resident bridge at all (rather than pure on-demand): the extension's native-messaging host is *spawned by the browser on first message and torn down when the port closes*, which already gives you lazy activation for free — **so the "bridge" can be the native-messaging host itself, and it can either talk to Pi directly or hand off to a socket-activated daemon.** Prefer the former (fewer processes); escalate to a daemon only if the indexer or multiple browser profiles need shared state. `[inferred]` This directly challenges the stated assumption: **do not keep a daemon running just because it is convenient.**

### R1. Idle policy (the core of "no permanent resource usage")

Adopt Pi's coordinator pattern, generalised (`experimental/coordinator.ts:263-264,483-506`):

- Bridge: `IDLE_EXIT_MS = 120_000` measured from last event **or** last open socket, whichever is later; timer is `unref()`'d so it can never itself keep the process alive. Re-arm on every event.
- Pi child: `CHILD_IDLE_EXIT_MS = 120_000` from last RPC command/stream end; bridge sends SIGTERM, then `killProcessTree` after 5 s (`utils/shell.ts:216`).
- Indexer: no idle timer — it exits when its work queue drains (bounded work, not bounded time).
- On Windows, "SIGTERM" is emulated by Node (`process.kill` on win32 terminates the process); use `taskkill /F /T` for tree semantics, per `utils/shell.ts:221`. `[verified]`→`[inferred]`

### R2. IPC per hop

| Hop | Choice | Rationale |
|---|---|---|
| extension → bridge | **Native messaging over stdio, JSONL framed identically to `modes/rpc/jsonl.ts`** | no listening port (no firewall prompt, no localhost attack surface), browser-managed lifecycle, lowest per-message overhead; falls back to **WebSocket on `127.0.0.1`** when native messaging cannot be installed (e.g. managed enterprise policy, Firefox MV3 differences) |
| bridge → Pi | **stdio JSONL** (`pi --mode rpc`) as the primary; **UDS / named pipe** only if a second client must attach | Pi already implements exactly this, including stdout-hijack protection (`core/output-guard.ts`) and backpressure |
| bridge → indexer | stdio JSONL (one-shot child) | simplest, no listener |
| bridge ↔ future UI/tray | UDS on Linux (`$XDG_RUNTIME_DIR/navi.sock`), named pipe `\\.\pipe\navi-<sid>` on Windows | matches `packages/server/src/transports/unix` semantics; **note Pi's UDS stack is POSIX-only today**, so NAVI needs a Windows transport if it wants cross-platform parity |

Trade-off summary (why stdio wins on the hot hops): stdio JSONL has no TCP handshake, no port allocation, no header parsing, and no firewall interaction; its cost is a `JSON.parse`/`stringify` per frame and a `write()` syscall. UDS/named pipes add a path/socket lookup and an ACL check but allow *many* independent clients and survive parent restarts. WebSocket adds HTTP upgrade, framing, and masking overhead and opens a local port — acceptable for the extension hop (no alternative), wasteful in-process. `[inferred]`, grounded in `modes/rpc/jsonl.ts`, `packages/server/src/transports/unix/listener.ts`, `experimental/coordinator.ts`.

### R3. Event batching / coalescing

Mirror the constants Pi already validated, and extend them for browser semantics:

| Event class | Coalesce window | Rationale |
|---|---|---|
| navigation / page-load commit | **0 ms** (immediate) | latency-critical, low frequency |
| DOM mutation, network, console | **250 ms** or 32 events, whichever first | matches the 100 ms bash flush (Pi) and 500 ms footer debounce order of magnitude; batches keep IPC frames/second bounded |
| scroll / selection / cursor | **500 ms**, and **drop intermediate** (keep last) | pure "where is the user" signal; stale positions are worthless |
| tab activate/deactivate | 16 ms (one frame) | ordering matters, frequency is low |
| clip/OCR of visible text | on **explicit user intent** only | the expensive one; never periodic |

All batches are **size-capped** and **sequence-numbered** so the bridge can detect drops rather than grow unbounded. Emit at most one JSONL frame per batch (`modes/rpc/jsonl.ts` framing). Pi's own numbers to anchor on: TUI render coalescing 16 ms (`tui.ts:477`), bash output 100 ms (`renderers/bash.ts:19`), git watcher debounce 500 ms (`footer-data-provider.ts:101`). `[verified]`

### R4. Context caching

- Reuse Pi's own accounting (`core/cache-stats.ts`) as a **first-class NAVI metric** and surface `missedTokens` / `missedCost` per turn.
- **Invariant:** the system prompt and tool schemas are byte-stable across turns; volatile ambient data (current URL, DOM excerpt, timestamps) goes into a **suffix** message, after the cache breakpoint, and is *never* re-serialised differently for identical content (stable key ordering, no `Date.now()` in cached regions). `[inferred]` from `CACHE_TTL_MS = 5 min` (`cache-stats.ts:8`) — any gap longer than 5 min or any front-of-prompt edit re-bills the whole prompt.
- Idle > 5 min implies cache expiry; therefore **do not keep a conversation "warm" by streaming filler** — the cache is gone anyway and the tokens are pure loss.

### R5. Background indexing schedule

- **Never periodic-poll.** Drive the indexer from coalesced event batches (R3) plus a **low-water debounce of 2 s** after the last change to a given resource.
- Incremental only: one resource at a time, checkpointed, so a killed indexer loses ≤ 1 item.
- At most **one** indexer process; if a new request arrives while one runs, it is queued (bounded queue, drop-oldest with a counter).
- Optional nightly full reconcile, gated on AC power + idle + no active session (Windows: `GetSystemPowerStatus`; Linux: `/sys/class/power_supply/*/status` + logind idle hint) — **and skipped entirely if the machine has been idle less than the threshold.** `[external]`/`[inferred]`

### R6. Model invocation frequency (LLM vs local heuristics)

Default posture: **local heuristics gate every LLM call.**
- Local-first classifiers (URL match, page title, regex over visible text, session state) answer the cheap questions: "is this a docs page?", "did the user just ask something?", "did this page change materially?".
- An LLM call requires one of: explicit user action, a **high-confidence** intent match from the local classifier, or a scheduled (user-configured) summarisation pass.
- Hard cap: at most **1 ambient LLM call per N minutes** (N configurable, default ≥ 5) and at most **1 in-flight** request; ambient calls are the first thing dropped under memory or battery pressure. `[inferred]`
- Reason: LLM calls are the only component with unbounded monetary and latency cost; everything else in NAVI is bounded and local.

### R7. Startup latency budget & suspend/resume

- **Bridge cold start:** readiness by first successful native-messaging round-trip; budget `TARGET p50 ≤ 250 ms`, `p95 ≤ 400 ms` for a Node-based host with a **deliberately tiny entry graph** (no TUI/theme/provider chunks). Reasoning: Node process spawn alone is typically 30–80 ms `[external]`; the rest is the host's own module graph, which NAVI controls.
- **Pi RPC child readiness:** budget `TARGET p50 ≤ 1.5 s`, `p95 ≤ 2.5 s` to a successful `get_state` response (exactly what `scripts/profile-coding-agent-node.mjs --mode rpc` measures). Reasoning: Pi's eager graph plus auth/model-runtime init dominates; the profiler exists precisely to push this down. `[inferred]` — **this number is the one to measure first**, not to trust.
- **Never block the browser on Pi start:** all user-visible work that can be done locally (highlight, extract, heuristics) proceeds while the Pi child boots; only true LLM turns await it.
- **Suspend/resume:** draining is `stop accepting new batches → flush queue to disk → SIGTERM child → wait ≤ 5 s → SIGKILL tree`. Resume is `spawn → readiness handshake → replay ≤ X pending items`. Linux may use `SIGSTOP`/`SIGCONT` for a short pause; **Windows cannot** (`interactive-mode.ts:4089-4092`), so there the only option is a real exit + restart. Because `idle-exit + restart` is the universal path anyway, **design suspend as "exit and resume from disk" and treat SIGSTOP as an optimisation, not a requirement.** `[verified]`→`[inferred]`

### R8. Storage footprint

- Everything under `$XDG_DATA_HOME/navi` (Linux) / `%LOCALAPPDATA%\navi` (Windows); sockets/pipes under `$XDG_RUNTIME_DIR` / `\\.\pipe\` respectively — never `/tmp` (world-writable) and never the CWD.
- Session transcripts: reuse Pi's append-only JSONL session format (already compact, already crash-safe).
- Index: bounded by a configurable byte cap with LRU-evicting segments; on cap breach, evict rather than grow.

---

## Alternatives

**A1 — Single always-on daemon that owns everything (bridge + Pi in one process).**
*For:* one process, in-process calls, no IPC at all for the bridge↔Pi hop.
*Against:* inherits Pi's eager module graph (`main.ts:8-71`) into the always-on footprint; a crash or leak in any subsystem kills the whole thing; RPC's "stay alive forever" (`rpc-mode.ts:819-820`) becomes a liability; you lose the ability to update one half.
**Verdict: reject as the default; keep as an internal fallback for a future "NAVI full" mode.**

**A2 — No daemon at all: the native-messaging host *is* the bridge, spawned per browser port.**
*For:* zero standing processes; the browser gives you lazy activation and idle teardown for free; smallest possible idle footprint.
*Against:* a fresh process per browser startup; no shared state across profiles; indexer work has nowhere to live.
**Verdict: recommended as the default (see R0); escalate only when a concrete need for shared state appears.**

**A3 — WebSocket-only IPC (extension → bridge → Pi).**
*For:* one code path, works from any client, eschews native-messaging install complexity.
*Against:* opens a localhost port (firewall prompts, non-browser malware reachability, DNS-rebinding class of problems), adds HTTP upgrade + frame overhead, and Pi's own RPC is stdio-native so an adapter is still needed.
**Verdict: keep as the *fallback* for the extension hop only; never use in-process.**

**A4 — UDS/named pipe as the *only* transport (reuse `pi-server`/`pi-client` directly).**
*For:* battle-tested in-repo (`packages/server/src/transports/unix/listener.ts`), ACL-friendly (mode 0600), multi-client.
*Against:* **POSIX-only today** (`packages/client/src/unix.ts:38,99` throw on Windows; `experimental-internal-process.test.ts:26` and `unix-transport.test.ts:54` are `skipIf(win32)`); the browser cannot reach it anyway.
**Verdict: use for the bridge↔Pi and bridge↔UI hops where it adds value, but ship a Windows named-pipe transport in parallel from day one.** Do not block the design on `pi-server` becoming cross-platform.

**A5 — Keep one warm Pi child resident forever and "just idle" it.**
*For:* lowest per-turn latency.
*Against:* directly violates the "avoid permanent resource usage when idle" requirement; Pi's own coordinator shows the intended pattern is to exit when empty; a resident child holds keep-alive HTTP sockets up to 300 s (`core/http-dispatcher.ts:4`) and a full module graph.
**Verdict: reject.**

**A6 — Poll for change (à la Pi's `watchFile` git-HEAD polling).**
*Against:* 250 ms/1 s stat loops are exactly the idle CPU/IO the requirement forbids; they are used in Pi only to survive rename-based atomic writes (`footer-data-provider.ts:313-315`).
**Verdict: reject as a design primitive; allow only as a bounded, self-cancelling fallback when native watching fails (mirroring `fs-watch.ts:3` retry semantics).**

---

## Risks

| # | Risk | Evidence / reasoning | Mitigation |
|---|---|---|---|
| R-1 | **Windows is under-tested.** Pi's suite runs on ubuntu only; Windows CI is `--help`/`--version`. | `ci.yml:14`; `build-binaries.yml:196-200` | NAVI ships its own `windows-latest` job covering the bridge, pipes, kill-tree, and idle-exit |
| R-2 | **Windows stdio-handle hangs** will bite NAVI's process supervision. | `utils/child-process.ts:38-137` exists solely for this (issue #5303); test `bash-close-hang-windows.test.ts:72` | Reuse the re-arming idle-grace pattern verbatim; always `taskkill /F /T` |
| R-3 | **No signals for suspend on Windows** → suspend logic must be exit+resume. | `interactive-mode.ts:4089-4092`; `rpc-mode.ts:368-370` (SIGHUP skipped on win32) | Design suspend as crash-safe resume-from-disk; never assume SIGCONT |
| R-4 | **UDS stack is POSIX-only in-repo.** A cross-platform bridge cannot depend on `pi-server` as-is. | `packages/client/src/unix.ts:38,99`; POSIX-only tests | Implement a thin named-pipe transport behind a shared `ByteConnection`-style interface; keep stdio JSONL as the portable default |
| R-5 | **Prompt-cache blowout from ambient context.** Volatile data at the front of the prompt re-bills the whole conversation. | `cache-stats.ts:8` (5-min TTL), `:138-163` (dollar accounting) | Volatile context appended after the cache breakpoint; byte-stable system prompt; no timestamps in cached regions |
| R-6 | **Unbounded event firehose → memory growth.** | Pi's every IPC path has explicit backpressure (`output-guard.ts:85-103`, `rpc-mode.ts:789`); NAVI has no equivalent yet | Bounded queues with drop counters; frame size caps; backpressure awaited before new reads |
| R-7 | **`fs.watch` semantics differ per OS** (recursive is natively supported on Windows/macOS but not Linux; file-level watches break on rename). | `footer-data-provider.ts:313-315`; `utils/fs-watch.ts` | On Linux, watch a bounded directory set explicitly and re-arm on inode change; never watch the whole disk; keep `watchFile` polling only as a last-resort fallback |
| R-8 | **No native clipboard under pure Wayland.** | `native-platform.ts:59-62` (requires `DISPLAY`) | Feature-detect and degrade; document; consider a portal-based path as a later enhancement |
| R-9 | **Extension capability gaps** (MV3 service-worker eviction, enterprise policy blocking native messaging, Firefox differences). | `[external]` | Ship both native-messaging and WebSocket paths behind one internal interface; never assume the extension can start processes |
| R-10 | **Two-lifetime coordination bugs** (race between "bridge thinks child is alive" and child exit). | `RpcClient` just sleeps 100 ms (`rpc-client.ts:132-139`) — no handshake | Explicit readiness frame + heartbeat + `exit` handler that invalidates the child handle |
| R-11 | **Native addon maintenance burden** grows with each target (3 OSes × 2 arches, two toolchains on Windows). | `native/win32/build.mjs:11-26` | Keep the native surface tiny and optional; JS fallbacks mandatory; prebuilds shipped, never built at install time |

---

## Performance budget table

All values are **design targets**, not measurements, unless a Pi constant is cited. `TARGET` = to be validated by prototype.

| Component | Metric | Target | How measured |
|---|---|---|---|
| **Browser extension** | Idle CPU | `TARGET` < 0.1 % (service worker parked) | `chrome://extensions` → "service worker (inactive)"; DevTools Performance on the worker |
| | Idle memory (per content script) | `TARGET` < 8 MB; background worker parked | Chrome Task Manager (Shift+Esc) → extension memory column |
| | Event emission rate (at rest) | `TARGET` ≤ 4 msg/s; burst ≤ 50/s, coalesced per R3 | instrument a counter in the extension; export via `chrome.runtime` telemetry to bridge |
| | Event batch coalescing latency | `TARGET` ≤ 250 ms p50 DOM/network; ≤ 500 ms p50 scroll | timestamp at source vs at bridge |
| **`navi-bridge`** | Cold start → ready | `TARGET` p50 ≤ 250 ms, p95 ≤ 400 ms | wrap start with `performance.now()`; log `METRIC bridge_ready_ms`; Linux `hyperfine` on the host binary |
| | Idle RSS | `TARGET` < 40 MB | Linux `ps -o rss= -p <pid>`; Windows `powershell -c "(Get-Process -Id <pid>).WorkingSet64"` |
| | Idle CPU | `TARGET` < 0.3 % of one core | Linux `pidstat -p <pid> 1 60`; Windows `typeperf "\Process(navi-bridge)\% Processor Time"` |
| | Idle exit latency after last event | `TARGET` 120 s ± 1 s, timer unref'd | unit test asserting the timer is unref'd; integration test observing process exit |
| | Resident processes at idle | `TARGET` = 1 (or 0 with A2) | `pgrep -fc navi`; Windows `Get-Process navi* | Measure-Object` |
| | Event queue depth bound | `TARGET` ≤ 1000 items, drop-counter exposed | unit test on the queue; counter in `get_state`-style status frame |
| **Pi runtime (`pi --mode rpc` child)** | Cold start → `get_state` response | `TARGET` p50 ≤ 1.5 s, p95 ≤ 2.5 s | **`cd packages/coding-agent && npm run profile:rpc -- --runs 10 --warmup 2`** → `METRIC startup_time_ms` (already implemented, `scripts/profile-coding-agent-node.mjs:645`) |
| | Startup phase breakdown | per-phase budget set after baseline | **`PI_TIMING=1 node dist/cli.js --no-session`** → `--- Startup Timings ---` block (`core/timings.ts:34-49`) |
| | Idle RSS (child resident) | `TARGET` baseline Node floor (~40–60 MB `[external]`) + session | Linux `ps -o rss= -p <pid>` before/after `prompt` |
| | Idle CPU (child resident, no turn) | `TARGET` < 1 % (must be ~0 between turns) | `pidstat -p <pid> 1 30` during a 30 s idle window |
| | Child idle → terminated | `TARGET` 120 s | bridge log + `pgrep -f "mode rpc"` after window |
| | RPC round-trip latency | `TARGET` p50 ≤ 15 ms, p95 ≤ 40 ms | harness sending N× `get_state` and recording p50/p95 (mirror `rpc-mode.ts:450` command) |
| | RPC frame throughput / backpressure | `TARGET` no unbounded growth under 10 MB streamed output | stream a large `bash` tool output; watch `process.memoryUsage().heapUsed` in the child |
| | Prompt-cache hit ratio per turn | `TARGET` ≥ 90 % of steady-state prompt tokens read from cache | reuse `core/cache-stats.ts` `computeCacheWaste` over the session JSONL |
| **`navi-indexer`** | Resident at idle | `TARGET` 0 processes | `pgrep -f navi-indexer` while idle |
| | Incremental lag (change → indexed) | `TARGET` p95 ≤ 5 s | timestamp injected at change source vs completion log |
| | Throughput | `TARGET` instrumented (no target until baseline) | `METRIC index_rate_items_per_s` emitted by the indexer |
| | Peak RSS while indexing | `TARGET` < 150 MB, hard cap → yield | `ps -o rss=` sampled at 1 Hz during a run |
| | Work bounded per wake | `TARGET` ≤ 30 s CPU, then yield/exit | wall-clock timer inside the indexer |
| **Storage** | Total footprint, 30 days typical use | `TARGET` < 200 MB | Linux `du -sh ~/.local/share/navi`; Windows `(Get-ChildItem -Recurse $env:LOCALAPPDATA\navi | Measure-Object Length -Sum).Sum` |
| | Index segment cap w/ LRU eviction | `TARGET` configurable, default ≤ 100 MB | `du -sh <index dir>`; unit test on eviction |
| | Session JSONL per turn | append-only, ≤ 1 frame/turn | inspect the JSONL file; reuse `docs/session-format.md` |
| **Whole system** | End-to-end "user action → response first token shown" | `TARGET` p50 ≤ 2.5 s (cold Pi child), ≤ 1.0 s (warm) | instrument at the extension UI; correlate with bridge + child timings |
| | Idle CPU (whole system, user browsing a static page) | `TARGET` < 0.5 % of one core | Linux `pidstat -p $(pgrep -f 'navi|mode rpc' | tr '\n' ',') 1 120`; Windows `typeperf` across `navi*` processes |
| | Idle RSS (whole system) | `TARGET` < 120 MB total when only the bridge is resident | sum of component RSS measurements above |
| | Wakeups/idle (Linux) | `TARGET` < 5 wakeups/s | `powertop` or `/proc/<pid>/status` voluntary_ctxt_switches delta over 60 s |
| | Battery impact (Windows) | `TARGET` categorized "Low" | Windows Sleep Study / `powercfg /energy` 60 s trace |

**Notes on the "how measured" column.** Four of these are directly available from Pi today with no new code: `npm run profile:tui`/`profile:rpc` (`package.json:37-38`), `PI_TIMING=1` (`core/timings.ts:6`), `PI_STARTUP_BENCHMARK=1` (interactive path, `main.ts:914-962`), and `--cpu-profile` CPU profiles (`profile-coding-agent-node.mjs:373-375`). Everything else requires NAVI-side instrumentation that does not exist yet and is listed as a prototype deliverable.

---

## Feature Assessment

| Feature | Verdict | Rationale (one line) |
|---|---|---|
| Event-driven bridge with unref'd idle self-shutdown | **CORE** | Pi's coordinator proves the pattern (`coordinator.ts:483-506`); it is the only way to satisfy "no permanent usage when idle". |
| Pi integration via `pi --mode rpc` child over stdio JSONL | **CORE** | Already implemented, backpressured, and profiled (`rpc-mode.ts`, `jsonl.ts`, `profile-coding-agent-node.mjs`). |
| Bridge-owned child lifecycle (idle timeout + tree kill) | **CORE** | Pi's RPC child intentionally never exits (`rpc-mode.ts:819-820`) — someone must own this. |
| Coalesced/batched event pipeline with bounded queues | **CORE** | Prevents the firehose-to-memory failure mode; mirrors Pi's flush throttles (100 ms/500 ms/16 ms). |
| Prompt-cache-stable context assembly + cache-waste metric | **CORE** | Direct spend lever; `cache-stats.ts` already computes it. |
| Per-entry module-graph budgets (CI-enforced) | **CORE** | `check-entry-graphs.mjs` shows the repo treats graph size as a cost contract. |
| Per-OS IPC abstraction (stdio default; UDS on Linux; named pipe on Windows) | **LIKELY** | Needed for parity, but stdio covers the hot path first. |
| Native messaging (stdio) as the extension↔bridge transport | **LIKELY** | Avoids a listening port and gives browser-managed lazy activation; some deployments will block it, hence "likely" not "core". |
| Localhost WebSocket fallback for the extension hop | **LIKELY** | Necessary escape hatch when native messaging is unavailable/policy-blocked. |
| Persistent bridge daemon (rather than per-port native host) | **LIKELY** | Only justified once shared state/multi-profile/indexer coordination is real. |
| On-demand `navi-indexer` child, never resident | **LIKELY** | Bulk work with no latency requirement; keeps idle footprint at zero. |
| Linux systemd user service + socket activation (`navi.socket`) | **EXPERIMENTAL** | Elegant lazy activation, but ties NAVI to systemd distros and complicates a portable installer. |
| Windows named-pipe transport with SDDL ACLs | **EXPERIMENTAL** | Required for daemon parity on Windows; Pi has no equivalent to copy. |
| inotify-based bounded directory watching on Linux | **EXPERIMENTAL** | Correct alternative to polling, but watch-limit budgets and recursive-walk cost need measurement. |
| Local heuristic gate in front of every ambient LLM call | **EXPERIMENTAL** | Right economics, but classifier quality/false-negative rate is unknown until prototyped. |
| Tray/notification UI (StatusNotifierItem on Linux, Win32 tray on Windows) | **EXPERIMENTAL** | Nice-to-have; adds a dependency and a second process/lifetime to manage. |
| Embedding-based semantic memory over browser history | **EXPERIMENTAL** | High value but unbounded storage/compute; needs a hard cap and a quality bar first. |
| Nightly full index reconcile gated on AC + idle | **DEFER** | Only worth building once incremental indexing is proven and measured. |
| Multi-profile / multi-user shared index | **DEFER** | Adds locking and privacy surface with no proven demand. |
| PGO/ICP or Rust rewrite of the native helper | **DEFER** | The helper is tiny and optional (`native/*/build.sh`); premature. |
| Cross-platform `pi-server`/`pi-client` port as the NAVI transport | **DEFER** | Valuable long-term but currently POSIX-only (`client/src/unix.ts:38,99`); do not block on it. |
| Passing `pino`-style structured logging + OpenTelemetry by default | **DEFER** | Pi's telemetry is no-op by default (`packages/telemetry/src/index.ts:24`); match that default. |
| Always-resident Pi TUI/agent process | **REJECT** | Violates the idle requirement and contradicts Pi's own empty-shutdown coordinator. |
| Periodic polling for change detection (250 ms–1 s `watchFile`) | **REJECT** | Exactly the idle CPU/IO the requirement forbids (`footer-data-provider.ts:336,370`). |
| WebSocket as the in-process/intra-host transport | **REJECT** | Adds a port, handshake, and framing overhead where stdio/UDS is strictly better. |
| Keeping the LLM stream/context "warm" during idle | **REJECT** | Cache expires at 5 min anyway (`cache-stats.ts:8`); pure token waste. |
| Bundling a full Node runtime per component | **REJECT** | Multiplies disk/RAM; use one runtime, split chunks (as `build-coding-agent-bundle.mjs` does). |
| Auto-updating components independently at runtime | **REJECT** (for v1) | Pi's own update path already needed a Windows exit-path workaround (`main.ts:588-594`); introduce only after the update story is designed. |

---

## Open Questions

1. **Does the browser hop *have* to be native messaging, or is an extension-hosted WebSocket acceptable for v1?** Determines installer complexity (registry/manifest files vs none) and whether NAVI needs a Windows installer story at all. `[inferred]`
2. **What is Pi's actual RPC cold-start `p50`?** The whole `TARGET ≤ 1.5 s` budget for the child rests on an unmeasured assumption. **Highest-value first measurement** — one command: `npm run profile:rpc -- --runs 10 --warmup 2`. `[unmeasured]`
3. **Can the Pi RPC child's eager graph (`main.ts:8-71`) be trimmed for `--mode rpc`?** If Pi upstream gains mode-conditional imports or a dedicated thin RPC entry, NAVI's child startup budget improves for free; otherwise NAVI may need to ship its own thin entry. `[inferred]`
4. **Is `pi-server`/`pi-client` intended to become cross-platform, or should NAVI fork the transport layer?** Affects whether the Windows named-pipe work is upstreamable or NAVI-local. `[inferred]`
5. **What is the acceptable ambient-LLM call rate?** This is a product/policy decision with a cost ceiling, not an engineering one; the R6 default (≥ 5 min, 1 in flight) is a placeholder. `[inferred]`
6. **Does NAVI need to survive browser restart, OS sleep, or only tab navigation?** Determines whether the bridge is native-messaging-host-only (A2) or needs a daemon (A1/A2+). `[inferred]`
7. **What is the target install size and platform matrix?** Pi ships 6 binary targets and 3 native prebuild sets; NAVI's copy of that burden should be an explicit, minimal decision. `[inferred]`
8. **Under what conditions is `fs.watch` acceptable on Linux** (watch limits vs recursion cost), i.e. what is NAVI's concrete watched-directory budget? No in-repo evidence answers this; needs a Linux prototype. `[inferred]`
9. **Should NAVI reuse `core/cache-stats.ts` semantics verbatim, or does the browser-ambient context pattern require a different cache-breakpoint strategy** (e.g. tool-result-style appended blocks)? `[inferred]`
10. **What is the Windows autostart vehicle** — `HKCU\...\Run`, Startup folder, scheduled task, or nothing (relying purely on browser-launched native messaging)? Pi has no desktop-autostart precedent to copy; this is greenfield. `[external]`/`[inferred]`
