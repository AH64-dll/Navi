# @navi/foundation

Private workspace for the NAVI CLI-owned browser bridge. It is **infrastructure, not a complete AI
product**. There are no model calls, no planning loop, no memory store, no screenshots and no site
tools. What exists is one validated browser path plus a diagnostic CLI.

Read the [foundation architecture](../../docs/navi-foundation/architecture.md) first for the product
boundary, the consent model and the deferred work. The [contributor guide](../../docs/navi-foundation/contributing.md)
covers module ownership, how to add a capability and the manual browser gates.

## Layout

```text
src/protocol/   versioned frames, strict runtime validation, fixed limits (browser-safe)
src/core/       browser-neutral ports, connection state, grants, CLI client (no Node/Chrome)
src/transport/  native-endian framing, bounded channels, private POSIX socket, relay connector
src/cli/        serve, diagnostics, native setup generator, terminal-safe output
browser/worker/ MV3 service worker: native port, sender authentication, grant authority, revocation
browser/content/ injected isolated collector, node reference table, guarded focus/scroll
browser/popup/  explicit attach, capability choice, status, disconnect
browser/platform/ narrow Chrome facade (no @types/chrome dependency)
test/           node:test suites for the Node realm
browser/test/   node:test suites for worker policy, collector and actions
```

Dependency direction is one way: `browser -> protocol + core`. Nothing imports the browser realm from
Node code, and core never imports Node or Chrome.

## Requirements

- Node >= 22.20 (the repo toolchain, with native type stripping for `.ts` entry points).
- A Chromium-based browser for the extension.
- POSIX local IPC. Windows named pipes are **not** implemented.

## Commands

Run from the repository root after `npm ci --ignore-scripts` (or `npm install --ignore-scripts`):

```bash
npm run check:navi                              # typecheck src + both browser projects
node --test packages/navi/test/*.test.ts        # protocol, core, transport, cli, security
node --test packages/navi/browser/test/*.test.ts # worker policy, collector, actions, security
node packages/navi/scripts/package-extension.mjs # build dist/extension (not installed)
```

## Running it manually

1. Start the browser side of the bridge:

   ```bash
   node packages/navi/src/cli/main.ts serve --socket-dir "$XDG_RUNTIME_DIR/navi"
   ```

   The directory must be absolute, owned by you and mode `0700`. `serve` creates the private socket
   and refuses to start if the path already exists, so a stale socket is a stop-and-check condition,
   not something the CLI silently removes.

2. Generate the native-messaging developer files. This writes into the directory you name and
   registers nothing:

   ```bash
   node packages/navi/src/cli/main.ts native-setup \
     --socket-dir "$XDG_RUNTIME_DIR/navi" \
     --extension-id <32-letter-id> \
     --out /tmp/navi-native
   ```

3. Load `packages/navi/dist/extension` as an unpacked extension, and register
   `/tmp/navi-native/dev.navi.bridge.json` in your browser's native-messaging host location yourself.
   The generator never edits your profile, your browser configuration or system paths.

4. Open the popup on the tab you want, choose the capabilities, and press Attach. `observe` is the
   default. `focus` and `scroll` are separate opt-ins.

5. In the `serve` terminal, use the diagnostic commands:

   ```text
   status | observe | focus <ref> | scroll <dy> | cancel | detach | quit
   ```

   `observe` prints the bounded observation as JSON. `focus` and `scroll` accept a reference from the
   latest observation. There is no model in the loop; you are the caller.

## What the foundation deliberately does not do

- No clicks, typing, form filling, navigation, file access, screenshots or arbitrary script
  evaluation. `focus` and `scroll` can still trigger site handlers and are observable by the page.
- No debugger, CDP, automation extension or anti-bot technique. Observation comes from DOM/ARIA, not
  Chrome's computed accessibility tree.
- No persistent grants, no automatic reconnect and no replay after a disconnect. Attaching again is a
  new explicit user action.
- No background capture. The collector runs only when `observe` is requested.
- No installers, no signing, no cross-browser packaging and no Windows transport.

## Verified here versus verified by you

Automated: framing, strict schemas, handshake and sequence rules, cancellation, grant revocation,
collector exclusions, action guards, permission surface and a real CLI-to-socket observe round trip
against a simulated worker.

Not automated: real native-messaging registration, real popup sender identity, BFCache and discarded
tab behavior, worker suspension, and real site `focus`/`scroll` side effects. Treat those as manual
gates before any release claim.
