# Cross-review 6 — Where NAVI's first surface should live

Adjudication over `vendor/pi` @ `71dca871` and reports 02/03/06/08. Positions: (1) Agent 8 terminal-first; (2) browser-extension-first; (3) Agent 8's multi-surface rules.

## Verdict

**First surface: the browser extension's side panel + badge, as a thin view over a session it does not own.** Reject terminal-first, desktop-first, tray-only, and multi-surface-from-day-1.

The model has three legs — ambient presence, summon, and a page-grounded answer. The third is a hard prerequisite: Pi has no browser/fetch/HTTP tool (`core/tools/index.ts:95-105,118-139`; nothing matching `fetch|browser|playwright` under `core/tools/**`), so page perception exists only if NAVI builds it, and no surface blind to the page can test the core bet. The extension is therefore mandatory under *every* option; the smallest surface proving the model reuses it. Every rival adds a second artifact (native shell) or front-end process (terminal) on top of the extension it still needs.

Fix three non-retrofittable decisions now: **session ownership stays in Pi** (a `pi --mode rpc` child carrying NAVI's gate/perception extension in-process, per 02/06); the panel holds views, never a message array. **Fan-out** is the extension background for browser surfaces only, NAVI's hub deferred. **Consent renders only in a trusted extension page** (side panel/popup), never a content-script overlay — a page can draw a pixel-identical fake (06 §5) — so the extension carries two rendering contexts at two trust levels.

Migration: **P1** extension perception + panel/badge. **P2** add the NAVI hub (SDK, owns the session) and a desktop shell (global hotkey, tray, toast, voice) as a second consumer of the same snapshot stream; the terminal becomes an optional third surface. **P3** formalize author/viewer/notifier roles + `get_entries{since}` resync.

## Evidence

| Option | Locks in | Cost | Never proves |
|---|---|---|---|
| **Browser extension panel** | session=Pi; browser fan-out=extension background; notify=browser badge/toast; consent=extension page (trusted); voice=`getUserMedia` | **M** — perception mandatory anyway; panel+badge marginal | OS presence; global summon with no browser foreground; native voice |
| **Desktop shell** | session=shell daemon; shell=hub; notify/consent/voice=native | **L/XL** — Pi has no globalShortcut/tray/electron/tauri (08 §1.8): a from-scratch native project *plus* the extension | Nothing structural — but it forces the native-render choice before the model is validated |
| **Terminal NAVI** | tempts session ownership into `InteractiveMode`; single-client RPC (no fan-out); notify=title/OSC; consent=terminal; no voice | **S** interaction / **M** daemon-first | Ambient presence in the user's real context; page-grounded answers; any non-terminal renderer |
| **Tray + notify only** | near-empty; notify path only | **S** | Summon *and* answer. False-ambient trap: `ServerLifetime` retires ~1s after last detach unless keepAlive (`server.ts:244-245,299-319`), and `notify` is unwrapped by `withUIPrompt`, so no `ui_prompt_*` fires (08 §1.9) |
| **Multi-surface day 1** | hub + fan-out + author-lock up front | **XL** | Nothing eventually — it validates the model last |

- **The floating-UI vocabulary is TUI-only.** `custom()`, `setFooter`, `setHeader`, `setWidget(factory)`, `setTheme` are the ambient kit (`types.ts:133-284`); over RPC `custom()`→`undefined` and `setFooter`/`setHeader` are no-ops (`rpc-mode.ts:210,214,228`; `docs/rpc.md:1195-1203`). Terminal-first spends P1 on a renderer every later surface replaces. Components are ANSI producers (`docs/tui.md:14-31`), and the `Terminal` seam that would reuse them (`terminal.ts:71`; `test/virtual-terminal.ts:11`) is unreachable from stock `InteractiveMode` (`interactive-mode.ts:352-373`).
- **The RPC cliff is misread by (1).** It degrades *extension-drawn* UI, not NAVI-drawn UI: a NAVI process renders its own panel from the RPC event stream on any transport. 08 §1.6(b) says exactly this ("every floating surface must be built by NAVI").
- **Ambient logic is transport-neutral.** `ui_prompt_start/end` (`runner.ts:441-480`), `agent_settled` (`types.ts:741`; `docs/rpc.md:905-911`), `queue_update` (`docs/rpc.md:1057`) all stream over RPC (`json-event.ts:18,46-61`); cost is identical per surface, only *where it is seen* differs.
- **Attribution correction:** 03 designs perception, 06 policy; neither picks a surface, and 03's Alt-B keeps the bridge first. Position (2) over-reads them — its surface claim rests on the concept's unverified "user lives in the browser" premise.

## Why the losing option remains attractive

Terminal-first is where Pi's toolkit already works — overlays, palette, focus, theme, summon shortcuts are free and proven (doom-overlay runs a 75%-wide overlay at ~35 FPS; `docs/tui.md:126-172,175-179`) — and the RPC cliff makes the terminal look like the only place rich UI can live. "Prove the model cheaply" is true for the *interaction* and false for the two legs that define NAVI: a terminal has no presence when unfocused beyond title/toast (08 §R2.7) and cannot sit beside the page being asked about. Worse, its cheap form — a NAVI extension in stock `pi` — locks session ownership into `InteractiveMode`; the daemon-first fix costs no less than going straight to the browser. The audience is terminal-native, so it *feels* cheap.

## Conditions / triggers to switch

- **Desktop-shell-first** if usage shows summon/questions occur mainly with no browser foregrounded — the "user lives in the browser" premise falsified.
- **Move consent off the extension** if the target browser lacks the needed permissions/side panel (Firefox/Zen/enterprise) → native dialog.
- **Pull multi-surface forward** if Pi adds an RPC version/readiness frame or promotes multi-attach out of `experimental`.
- **Add the terminal as a third surface** only once the daemon owns the session.
- **Presence-first (tray/desktop)** if the ambient state machine proves to be the product and page-answering commodity.

## Confidence

**High** on the verified constraints (TUI-only UI kit; RPC degradation; no OS integration in Pi; tray false-ambient trap; transport-neutral ambient events). **Medium** on the surface verdict, which rests on one unverified premise — that NAVI's user is browser-centred. If wrong, desktop-shell-first inverts it; the phase-1 architecture survives regardless.
