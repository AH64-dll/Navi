# NAVI UX / Interface Architecture — Investigation 08 (Agent 8 of 10)

**Subject:** `C:\Users\lain\NAVI\vendor\pi` — read-only clone of `github.com/earendil-works/pi`
(branch `main`, commit `71dca871`, 2026-09-11; `@earendil-works/pi-coding-agent` v0.85.1).

**Scope:** design research only. No implementation. No files in the repo were modified.
All in-repo citations are relative to `packages/` unless a full path is given.

**Evidence tags:** `[verified]` = read in source or first-party docs in this clone;
`[inferred]` = reasoned from verified evidence; `[external]` = general prior knowledge, not
in this repo.

**Assumptions stated up front**

1. "NAVI" is a new harness built *around* Pi as the agent engine, not a fork of Pi's
   `InteractiveMode`. Any statement that NAVI "uses" a Pi API assumes that API is public
   (`@earendil-works/pi-coding-agent` / `@earendil-works/pi-tui` / `@earendil-works/chord`).
2. "Surface" = a user-visible presentation (terminal TUI, desktop overlay, tray, browser
   panel, voice) attached to the *same* agent session.
3. Pi has no audio, no browser extension, no OS-level hotkey, no HTTP server for UI. Those
   are therefore NAVI obligations, not Pi capabilities — stated explicitly where relevant.

---

## Repository Findings

### 1. What Pi can render today

**1.1 The component model is terminal-string based, and injectable.**

- `packages/tui/src/tui.ts:425` → `export interface TUI extends Component` with
  `setFocus(component)` (`:438`), `requestRender(force?)` (`:445`), `addInputListener(listener)`
  (`:446`) → every rendering unit in Pi is `{ render(width): string[], handleInput?, invalidate() }`.
  `docs/tui.md:14-21` documents exactly this: `render(width)` returns an array of ANSI lines and
  **each line must not exceed `width`**. → A non-terminal frontend cannot consume Pi's
  components directly; it must consume the *event/state* layer instead (see §1.6).
- `packages/tui/src/index.ts:22-45` → exports `ScrollView`, `VStack`, `MouseRegion`, `Image`,
  `Markdown`, `SelectList`, `SettingsList`, `Box`, `Text`, `Input`, `Editor`, `Loader` → Pi
  ships a small, complete widget kit (scroll viewport, vertical stack, mouse regions, inline
  images, markdown, lists, editors). NAVI does not need to build a layout engine.
- `packages/tui/src/tui.ts:456` → `ViewportTUI extends TUI` with `setLayoutRoot(component)`
  (`:458`) → **a viewport/alt-screen TUI can be composed by a third party**: pick a layout root,
  focus a component, start it. This is the exact seam `src/experimental/client-tui.ts:781-785`
  uses (`tui.addChild(component)`, `tui.setLayoutRoot(...)`, `tui.setFocus(component)`, `tui.start()`).
- `packages/tui/src/terminal.ts:71` → `export interface Terminal { start(onInput, onResize); stop();
  drainInput(); write(data); moveBy(); hideCursor(); showCursor(); clearLine(); clearFromCursor();
  clearScreen(); setTitle(); setProgress(); ... }` → **the byte sink is an interface**.
- `packages/tui/test/virtual-terminal.ts:11` → `export class VirtualTerminal implements Terminal`
  → a headless/bespoke `Terminal` implementation already exists in-tree and is exercised by
  `packages/tui/test/tui-render.test.ts` (`BoundedWriteTerminal implements Terminal`, `:43`).
  → "render Pi's TUI into a non-TTY sink (e.g. xterm.js in a browser, a compositor surface) is a
  supported seam, not a hack." `[verified seam]` `[inferred capability]`
- `src/modes/interactive/tui-renderer.ts:8-15` → `InteractiveTuiOptions { tuiMode: "regular" |
  "fullscreen"; showHardwareCursor; logDirectory; terminal?: Terminal; ... }`, and `:21-48`
  `createInteractiveTui()` returns `TuiAltScreen` (fullscreen, own viewport, mouse capture,
  search, copy-on-select, OSC 8 open-url) or `TuiMainScreen` (regular, native scrollback).
  → **there are exactly two presentation modes**, `regular` and experimental `fullscreen`
  (`docs/settings.md:69`), and both accept an injected `Terminal` (`:22`).
  Note: `InteractiveModeOptions` (`src/modes/interactive/interactive-mode.ts:352-373`) does *not*
  expose `terminal` — only `tuiMode`. The repo's own reuse of a custom terminal is the
  mode-switch path at `interactive-mode.ts:806,819,823`. So injecting a custom `Terminal` into the
  *stock* `InteractiveMode` is **not** supported today. `[verified]`

**1.2 The layout Pi ships: fixed input dock under a scrolling transcript.**

- `src/modes/interactive/chat-viewport.ts:22` → `createChatViewport(options)` builds
  `ScrollView(document, { follow: "end", primary: true, overscroll: "chain", scrollbar })` plus a
  `VStack` dock of `[pendingMessages, status, widgetsAbove?, editor(minSize:3), widgetsBelow?,
  footer]`, and returns `{ root, transcript }` → **the entire ambient chrome of the app is
  addressable**: pending-message queue, status row, two widget slots, editor, footer. NAVI's
  "ambient bar" and "radar line" have pre-existing, supported mounts.
- `examples/extensions/widget-placement.ts:5-8` → `ctx.ui.setWidget("above", [...])` and
  `setWidget("below", [...], { placement: "belowEditor" })` → both widget slots are reachable
  from an extension with nothing but strings.

**1.3 What an extension can put on screen (`ExtensionUIContext`).**

`src/core/extensions/types.ts:133-284` → `ExtensionUIContext`. The full inventory, with the
implication for NAVI:

| Method (line) | Capability | NAVI surface it enables |
|---|---|---|
| `select` `:135`, `confirm` `:138`, `input` `:141`, `editor` `:224` | blocking dialogs, each takes `{ signal, timeout }` (`ExtensionUIDialogOptions`, `:98`) | approval gate, timed "signal detected" prompt with countdown |
| `notify(msg, "info"\|"warning"\|"error")` `:144` | fire-and-forget notification | OS/desktop toast surface |
| `onTerminalInput(handler)` `:147` | **raw terminal bytes**, handler may return `{consume, data}` | global *in-app* hotkey listener, typed-ahead capture |
| `setStatus(key, text\|undefined)` `:150` | keyed footer status entries | ambient state readout (idle/listening/streaming) |
| `setWorkingMessage/Visible/Indicator` `:153/:156/:166` | control the streaming loader (`WorkingIndicatorOptions`, `:118`, frames+intervalMs) | the "signal processing" cursor pulse |
| `setHiddenThinkingLabel` `:169` | relabel hidden thinking | "inference trace" affordance |
| `setWidget(key, string[] \| factory, {placement})` `:172-177` | **component factory allowed** (`(tui, theme) => Component`) | radar line, graph legend, ambient ticker |
| `setFooter(factory)` `:185`; `setHeader(factory)` `:192` | replace footer / header entirely, receive `ReadonlyFooterDataProvider` | NAVI header (state + counts), NAVI footer (affordances) |
| `setTitle(title)` `:195` | terminal tab/window title | background presence while unfocused |
| `custom<T>(factory, { overlay?, overlayOptions?, onHandle? })` `:198-212` | **full-screen or floating component with keyboard focus**, returns any value | command palette, side panel, graph canvas |
| `pasteToEditor` `:215` / `setEditorText` `:218` / `getEditorText` `:221` | programmatic editor I/O | seed-as-context, "load answer into editor" |
| `addAutocompleteProvider(factory)` `:227` | stack on the built-in provider | `#`, `@` NAVI triggers |
| `setEditorComponent(factory)` `:262` / `getEditorComponent` `:265` | replace the input editor | modal/vim feel, prompt-as-console |
| `theme` `:268`, `getAllThemes` `:271`, `getTheme` `:274`, `setTheme` `:277` | theme objects at runtime (JSON files, `docs/themes.md`) | **NAVI's dark cyan/blue palette is a theme JSON, not code** |
| `getToolsExpanded`/`setToolsExpanded` `:280/:283` | tool output density | "calm" vs "verbose" |

- `docs/tui.md:126-172` → `{ overlay: true, overlayOptions: { width: "50%", minWidth, maxHeight:
  "80%", anchor: "right-center", offsetX, offsetY, row/col, margin, visible(termWidth, termHeight) } }`
  → **floating, anchored, responsive panels are first-class**, with a `visible` predicate used for
  narrow-terminal degradation (`docs/tui.md:161`).
- `docs/tui.md:175-179` and `docs/extensions.md:2802` → overlay *focus* semantics: a focused
  visible overlay keeps input ownership across temporary non-overlay UI; `handle.focus()`,
  `handle.unfocus({target})`, `handle.setHidden(bool)`, `handle.hide()` → **a panel can stay on
  screen while the user keeps typing in the editor.** This is the single most important primitive
  for an ambient UI (see §2.1).
- `docs/tui.md:183-200` → overlay components are disposed on close; you must re-`custom()` to
  re-show → NAVI must keep overlay *state* outside the component instance `[verified footgun]`.
- `examples/extensions/doom-overlay/index.ts:57-66` → an overlay at `width:"75%",
  maxHeight:"95%", anchor:"center", margin:{top:1}` running a real-time game at ~35 FPS
  (stated in the file header, `:9`) → **overlays can be continuously animated and input-hungry**;
  the overlay is not merely a modal dialog. `[verified]`
- `examples/extensions/overlay-test.ts:33-190` → `class OverlayTestComponent implements
  Focusable`, sets `focused`, emits `CURSOR_MARKER` for IME, renders embedded text inputs inside
  overlay rows → overlays can host their own inline editors with hardware-cursor positioning.

**1.4 Widget/status/header/footer/title in practice (each example is 20–70 lines).**

- `examples/extensions/custom-header.ts:60-77` → `setHeader((_tui, theme) => ({ render, invalidate }))`
  replaces the logo block → NAVI's idle-state mark lives in the header.
- `examples/extensions/custom-footer.ts:26-56` → `setFooter((tui, theme, footerData) => ...)` with
  `footerData.getGitBranch()`, `footerData.onBranchChange(cb)` for reactive redraw, and token
  totals recomputed from `ctx.sessionManager.getBranch()` → **footer is a live dashboard with a
  push notification channel for external state changes.**
- `examples/extensions/status-line.ts:11-28` → `setStatus` on `session_start`/`turn_start`/`turn_end`
  → the minimal turn-progress readout.
- `examples/extensions/titlebar-spinner.ts:41-58` → braille spinner via `setTitle` on `agent_start`
  and stopped on `agent_settled` → **the terminal title is a legitimate 1-line background status
  surface.**
- `examples/extensions/notify.ts:11-52` → on `agent_settled`, desktop notification via OSC 777
  (Ghostty/iTerm2/WezTerm), OSC 99 (Kitty) or a Windows toast through `execFile("powershell.exe")`
  → **Pi extensions can already raise OS-level notifications portably.** The file's own comment
  (`:43-45`) explains why it listens to `agent_settled` rather than `agent_end` (retries,
  compaction, queued follow-ups) → "done" semantics must use `agent_settled`.
- `examples/extensions/minimal-mode.ts:63-127` → tool *renderers* return `new Text("", 0, 0)` when
  collapsed → **an entire category of chrome (tool call bodies) can be made invisible without
  losing function.** Ambient-by-default is achievable today.

**1.5 Message/entry rendering and durable, non-LLM UI state.**

- `src/core/extensions/types.ts:1352` → `registerMessageRenderer(customType, renderer)`;
  `:1355` `registerMarkdownTransformer`; `:1358` `registerEntryRenderer(customType, renderer)`;
  `MessageRenderer` `:1213`, `EntryRenderer` `:1219` (both `(item, options, theme) => Component | undefined`).
- `:1381` → `appendEntry(customType, data)` "Append a custom entry to the session for state
  persistence (**not sent to LLM**)" → **durable UI state that survives resume and never pollutes
  context.**
- `examples/extensions/entry-renderer.ts:19-38` → `appendEntry("status-card", {...})` +
  `registerEntryRenderer("status-card", ...)` renders a boxed card that is not in the LLM context.
  → the correct home for NAVI's research-graph nodes, radar snapshots, and annotation records.
- `examples/extensions/message-renderer.ts:16-34` → `sendMessage({ customType, content, display,
  details })` + custom renderer → in-context but custom-drawn turns.
- `examples/extensions/structured-output.ts:11-40` → `defineTool({... validate, terminate: true })`
  and `execute` returns `terminate: true` → **an agent turn can end on a structured tool call with
  no extra LLM round-trip** → the mechanism for "answer as data, render it my way" (graph, card,
  citation list) rather than "answer as prose".
- `src/core/extensions/types.ts:866-877` → `InputEvent { text, images, source: "interactive"|"rpc"|
  "extension", streamingBehavior }` and `InputEventResult = continue | transform | handled`
  (`:880-883`) → **input can be intercepted, rewritten, or swallowed before it reaches the agent.**
  `:874` records `source`, so a surface can know its input came from an RPC/ambient channel.

**1.6 Two genuinely different frontend architectures exist in-tree.**

**(a) In-process SDK.** `docs/sdk.md` → `createAgentSession()`, `AgentSessionRuntime`,
`session.subscribe(listener)` for `agent_start/agent_end/agent_settled/turn_*/message_*/tool_execution_*`,
`session.prompt/steer/followUp`, `session.setModel`, `session.exportToHtml`
(`src/core/agent-session.ts:3463`), extension loading via `DefaultResourceLoader`,
`runtime.newSession/switchSession/fork`, and explicit caveats: *"event subscriptions are attached
to a specific AgentSession, so re-subscribe after replacement"* and *"if you use extensions, call
`runtime.session.bindExtensions(...)` again"* (`docs/sdk.md`, AgentSessionRuntime section).
`[verified]` → **the SDK is the only path that gives NAVI one long-lived process owning the
session, with full event granularity and no IPC.**

**(b) RPC over stdin/stdout.** `docs/rpc.md:1-26` → `pi --mode rpc`; commands are JSONL on stdin,
responses/events are JSONL on stdout (strict LF framing, `:30-37` — Node `readline` is *not*
protocol-compliant). Commands: `prompt` with `images`, `steer`, `follow_up`, `abort`,
`clear_queue`, `new_session`, `get_state`, `get_messages`, `get_tree`, `get_entries` (with a
durable `since` cursor, `:719`), `get_last_assistant_text`, `set_session_name`, `compact`,
`bash`, `export_html`, `switch_session`, `fork`, `clone`, `get_commands` — all with optional `id`
correlation. Events: the table at `:861-884` including `agent_settled`, `message_update`
delta-only (`:938-996`, explicitly *"omits the cumulative message field"* — clients must assemble
from `message_start` + deltas, `toolcall_delta` for args), `tool_execution_update` carrying
*accumulated* `partialResult` (`:1055`).

**The critical asymmetry:** `docs/rpc.md:1184-1205` → RPC extension UI is a *degraded subset*:
`custom()` returns `undefined`; `setWorkingMessage/Indicator`, `setFooter`, `setHeader`,
`setEditorComponent`, `setToolsExpanded` are **no-ops**; `getAllThemes()` → `[]`; `getTheme()`
→ `undefined`; `setTheme()` fails. Only `select/confirm/input/editor/notify/setStatus/setWidget
(string arrays only — component factories ignored, `:1324`)/setTitle/set_editor_text` survive.
`ctx.mode === "rpc"`, `ctx.hasUI === true` (`:1205`, `docs/extensions.md:2943`).
→ **If NAVI's frontend is not a real terminal, every floating surface (palette, side panel,
overlay, radar, graph) must be built by NAVI. Pi will only give you events + five dialog
round-trips + text lines.**

**(c) A third, in-tree proof that a custom frontend can drive Pi remotely.**
`src/experimental/client-tui.ts:727` → `runClientTui()` composes its own TUI:
`createInteractiveTui({ tuiMode: "fullscreen", ... })` (`:743`), `createChatViewport({...})` (`:143`),
`new CustomEditor(...)` (`:130`), subscribes to a *replicated lane snapshot*:
`feature.transcript.state.subscribe((value) => { view.apply(value.snapshot); this.#rebuild(); })` (`:501-505`),
and sends turns through `AgentController.prompt/steer/followUp`
(`:563-585`) with `requestAbort` (`:607`). It is a **pure presentation client**:
`src/experimental/client-tui-chat.ts:21` → `ExperimentalChatView` with `apply(snapshot)` (`:40`),
`#syncTranscript` (`:85`), `#syncStreaming` (`:136`), `#syncQueues` (`:76`).
`src/experimental/client-runtime.ts:53` → `openClientRuntime()` connects to servers over a **unix
socket** (`createUnixTransportFactory`, `:4`) or a Radius relay (`:129`), and
`:186` `activateBuiltinClientServices()` acquires `SessionDirectory, SessionManagement,
PresentationPlugins / Models, AgentController, Transcript`.
→ **"frontend renders replicated state, agent runs elsewhere" is not hypothetical; it is
implemented.** `[verified]`

**1.7 Multi-surface / one-session semantics are already modelled (experimentally).**

- `src/experimental/server.ts:248` → `class ServerLifetime` refcounts **connections and session
  workers**; retires the server only when both are zero, after
  `AUTO_SERVER_IDLE_GRACE_MS = 1_000` (`:245`), with a 10 s startup grace (`:244`) and a
  `keepAlive` escape hatch (`:259`, and `StartServerOptions.keepAlive`, `:344-345`).
  → **"background presence" has a precise meaning in Pi: hold a connection, or run the server with
  `keepAlive`.** Without that, the daemon dies ~1 s after the last client detaches.
- `src/experimental/session-worker-manager.ts:212-213` → `#attachClient()` mints a fresh
  `attachmentId` (`randomUUID()`) and adds it to `worker.attachmentIds` (a `Set`, `:54`), with
  per-attachment operation scope (`:220`) and independent service subscriptions (`:253-258`).
  → **N concurrent attachments to one session worker; each gets its own scoped service
  subscriptions and its own replicated-state stream.** `[verified]`
- `src/experimental/services/transcript.ts:4-15` → `TranscriptState { snapshot: LaneTranscriptSnapshot | null }`
  published through Chord's operation stream ("Coherent main-lane state replicated through Chord's
  operation stream") → one authoritative lane, many coherent replicas.
- `src/experimental/services/connection.ts:24-25` → attachment state machine
  `detached | attaching | attached | degraded`; `:19-21` connection state
  `connecting | connected | disconnected`. → the UX states NAVI must render ("reconnecting to
  Radius", "reattaching session") already exist as data; `client-tui.ts:447-492` renders them.
- `src/experimental/services/sessions.ts:28-35` → `SessionManagement { create, remove, attach,
  detach }` → attach/detach is a first-class session operation, not a process lifecycle.
- `packages/chord/README.md:10-27` → Chord exists specifically because *"A single application
  feature may need to run in several environments: for example an agent worker, a terminal UI,
  and **a remote WebUI**"*, and facets are *"bundled up separately and run in the process or
  environment where it's supposed to run… backend, browser, TUI"*. `:34-44` replicated state;
  `:124-178` facet bundling with SHA-256 verification and hot-cutover reload
  (`FacetHost.reload()`).
  → **a browser presentation facet is an anticipated, designed-for case**, and
  `src/experimental/plugins/bundled.ts:32-45` (`createPresentationFacetData` /
  `createPresentationFacetLoaders`) is the in-tree plumbing for loading presentation plugins per
  session. `[verified]` (experimental)
- `src/experimental/plugins/package.ts` + `docs/packages.md` + `examples/plugins/` → plugin
  packages are installable per session.

**1.8 What does *not* exist (verified negatives — these are NAVI obligations).**

- `grep -i 'globalShortcut|systemTray|menubar|electron|tauri'` over all `packages/**/src/**/*.ts`
  → **no matches.** There is no OS-global hotkey, no tray icon, no desktop-shell integration in Pi.
- No WebSocket/HTTP server for UI: the only `listen(` matches are
  `packages/server/src/transports/unix/listener.ts` (unix socket) and
  `src/experimental/radius-relay.ts` / `mini/shared/transport.ts` (relay transports). The web
  surface Pi *does* have is one-way and static: `/share` exports HTML
  (`src/modes/interactive/session-share.ts:46,72` → `session.exportToHtml`), uploads it to
  `https://…/v1/artifacts` on the Radius gateway (`:108-135`) or a private gist (`:148-196`) and
  opens `getShareViewerUrl(gistId)` (`:191`, overridable via `PI_SHARE_VIEWER_URL`,
  `docs/environment-variables.md:88`). → **a read-only web viewer for a session exists; an
  interactive web surface does not.**
- No audio/STT/TTS anywhere. Voice is entirely NAVI's problem.
- RPC mode is a single stdin/stdout pipe per process (`docs/rpc.md:20-26`) → **one frontend per
  RPC process**; N surfaces over RPC require NAVI to run a hub that is the sole writer of stdin.

**1.9 Attention/human-needed signalling.**

- `src/core/extensions/types.ts:745-761` → `UIPromptKind = "select" | "confirm" | "input" |
  "editor" | "custom"`, `UIPromptStartEvent` (`:748`) and `UIPromptEndEvent` (`:756`).
- `src/core/extensions/runner.ts:441-480` → `wrapUIPromptContext()` wraps exactly those five
  methods in `withUIPrompt()` (`:444-449`), which emits `ui_prompt_start` when
  `uiPromptDepth++ === 0` (`:454-458`) and `ui_prompt_end` on completion (`:460-472`), via
  `queueMicrotask` (`:483`). `setUIContext(ui, mode)` (`:436`) is mode-agnostic.
  → **Pi tells you when it has stopped and needs a human, with the prompt kind and title, in every
  mode.** This is the natural "signal detected" trigger, and it is the *only* such signal:
  `notify` is deliberately not wrapped. `[verified]`
- `agent_settled` (`types.ts:741`, `docs/rpc.md:905-911`) → "no automatic retry, compaction, or
  queued continuation will run" → the only correct "idle/ready" edge.

### 2. Theming, keybindings, and where NAVI's look actually lives

- `docs/themes.md` → themes are JSON with **53 required tokens** (`accent`, `border`,
  `borderAccent`, `borderMuted`, `success/error/warning`, `muted`, `dim`, `text`, `thinkingText`,
  `scrollbarTrack/Thumb`, `selectedBg`, `searchMatchBg/Text`, `userMessageBg/Text`,
  `customMessageBg/Text/Label`, `toolPending/Success/ErrorBg`, `toolTitle`, `toolOutput`, 10
  markdown tokens, 3 diff tokens, 9 syntax tokens, 7 thinking-level border tokens, `bashMode`),
  plus optional `export` colors for HTML export. `vars` allows a named palette; hex / 256-index /
  var / `""` (terminal default) values; live hot-reload on edit.
  → **NAVI's dark cyan/blue, restrained typography is a theme file plus a header/footer/widget
  implementation — no core changes.** `thinkingOff→thinkingMax` is a ready-made 6-step
  "intensity ramp" that maps cleanly onto a signal-strength metaphor. `[verified]`
- `docs/keybindings.md` → namespaced ids (`tui.editor.*`, `tui.input.*`, `tui.select.*`,
  `tui.altScreen.*`, `app.*`), user overrides in `~/.pi/agent/keybindings.json`, applied on
  `/reload`. `app.interrupt`=escape, `app.clear`=ctrl+c, `app.exit`=ctrl+d, `app.model.select`=ctrl+l,
  `app.tools.expand`=ctrl+o, `app.message.followUp`=alt+enter, `app.session.tree` (**unbound**),
  `tui.altScreen.search`=`ctrl+shift+f`, `tui.altScreen.bottom`=end.
  `docs/keybindings.md:89-108` → in fullscreen mode unmodified nav keys belong to the transcript
  and `ctrl`-variants to the editor; transcript search has its own panel with clickable controls.
  `src/core/extensions/types.ts:1320` → `registerShortcut(shortcut: KeyId, {description, handler})`
  (implementation: `src/modes/interactive/interactive-mode.ts:2031-2085`, dispatched through
  `defaultEditor.onExtensionShortcut` at `:2072`, running the handler async so input is not blocked).
  → **NAVI can claim a summon key (e.g. `ctrl+space`) and it will appear in `/hotkeys`; it is an
  in-terminal shortcut only, active when Pi has focus.** `[verified]`
- `src/modes/interactive/interactive-mode.ts:2382-2391` → `addExtensionTerminalInputListener`
  registers a handler via `ui.addInputListener(handler)` and can return `{consume: true}`
  → below-shortcut-layer interception; the provider of *global* keystrokes inside the app.
- `docs/tui.md:312-326` → fullscreen-only mouse: press/release/click/move/drag/wheel routed to
  components and overlays, `MouseRegion` for hit regions, OSC 8 links take precedence, unhandled
  wheel scrolls nearest `ScrollView`. Regular mode deliberately does **not** capture the mouse
  (terminal owns scrollback). → **rich pointer affordances are fullscreen-only.**
- `docs/tui.md:34-87` → `Focusable` + `CURSOR_MARKER` (zero-width APC) for hardware-cursor/IME
  positioning, and the container-must-propagate-`focused` rule → matters for any NAVI input inside
  an overlay.
- `docs/themes.md` export section + `session.exportToHtml` (`agent-session.ts:3463`) + `setTheme`
  in RPC failing → **HTML/web presentation is an export-time concern in Pi, not a live surface.**

### 3. Mode matrix (the constraint that shapes everything)

`docs/extensions.md:2940-2947`:

| Mode | `ctx.mode` | `ctx.hasUI` | What UI works |
|---|---|---|---|
| Interactive | `tui` | true | everything in `ExtensionUIContext` |
| RPC | `rpc` | true | 5 dialogs + notify/setStatus/setWidget(strings)/setTitle/set_editor_text |
| JSON | `json` | false | UI methods are no-ops |
| Print | `print` | false | extensions run, cannot prompt |

`src/core/extensions/types.ts:307` → `ExtensionMode = "tui" | "rpc" | "json" | "print"`;
`:315` → `hasUI`. Every non-TUI example guards with `if (ctx.mode !== "tui")` before using
`ctx.ui.custom` (`examples/extensions/qna.ts:30-33`, `question.ts:54-56`,
`doom-overlay/index.ts:26-28`).
→ **A NAVI extension that ships floating UI must be written against the TUI mode, or the UI must
live in NAVI's frontend. There is no middle path.** `[verified]`

### 4. Real-time and progress surfaces available to a NAVI shell

- Streaming: `message_update` with `text_start/delta/end`, `thinking_start/delta/end`,
  `toolcall_start/delta/end` (`docs/rpc.md:961-996`); `tool_execution_update` with the accumulated
  `partialResult` (`:1055`) → a live "trace" renderer is straightforward.
- Queue: `queue_update { steering, followUp }` (`docs/rpc.md:1057`), `clear_queue` returning the
  text so Esc can restore it to the editor (`:158`) → Esc semantics are specified, not invented.
- Compaction/retry: `compaction_start/end` with `reason: manual|threshold|overflow` (`:1077`),
  `auto_retry_start/end` (`:1109`), `summarization_retry_*` (`:1141`) →
  `StatusIndicatorKind = "working" | "retry" | "compaction" | "branchSummary"`
  (`src/modes/interactive/components/status-indicator.ts:7`) is the authoritative list of
  background states worth showing.
- Graph/tree: `get_tree` returns `{entry, children, label?, labelTimestamp?}[]` (`docs/rpc.md:747-773`)
  → **the session tree is a directly fetchable graph** (nodes = entries, edges = parentId).
  `get_entries` with a durable `since` cursor (`:719`) is an incremental sync primitive.
- `docs/sdk.md` ToolRenderContext / `src/core/extensions/types.ts:421-446` →
  `args, toolCallId, invalidate(), lastComponent, state, cwd, executionStarted, argsComplete,
  isPartial, expanded, showImages, isError` → per-tool live rendering with stable identity.

---

## Architectural Implications

**A1. Pi's UI layer is terminal-shaped; its *state* layer is frontend-neutral.**
Components emit ANSI strings (`docs/tui.md:14-31`), so a browser/GUI frontend must not try to
reuse them. But everything that *drives* the UI — the event stream, the session tree, the
transcript entries, the agent controller, the replicated lane snapshot — is plain data
(`docs/rpc.md:855+`, `client-tui-chat.ts:40-56`). Consequence: **NAVI should define its own
presentation components for non-terminal surfaces, and treat Pi as a state/event source.**
The one exception is the `Terminal` seam (§1.1), which lets NAVI reuse *all* of Pi's TUI by
supplying a non-TTY sink.

**A2. There is exactly one place where Pi stops being a data source and needs a human:**
`ui_prompt_start` / `ui_prompt_end` with `kind` and `title` (`runner.ts:441-480`). Everything else
is asynchronous state. Therefore the "signal detected" moment in NAVI should be **built on
`ui_prompt_start` + `agent_settled` + `queue_update`**, not on a timer or a heuristic.

**A3. Multi-surface with one session is a *transport* decision, not a UI decision.**
The base harness gives you one frontend per process (`rpc`) or one process total (`SDK`). The
experimental server gives you N attachments to one worker with per-attachment scopes and coherent
replicated state (`session-worker-manager.ts:205-239`, `transcript.ts:4-15`) — but it is labelled
experimental throughout and its own client (`client-tui.ts`) is the only consumer. So NAVI must
choose: **own the fan-out (SDK daemon + NAVI's own hub), or adopt Pi's experimental server and
inherit its churn.** [inferred from §1.7]

**A4. Prompt authority must be explicitly owned by one surface.**
Nothing in Pi prevents two attachments from sending prompts (`AgentController.prompt/steer`,
`client-tui.ts:563-585`); the *queueing semantics* assume one narrator
(`steer` = after current turn's tools, `followUp` = when the agent stops; `docs/rpc.md:56-63`).
So NAVI must designate a **primary/authoring surface** and make others subscribe-only for
prompting. Otherwise "the browser asked a question, the terminal also asked something, which
steer applied to which turn?" becomes unanswerable. [inferred]

**A5. Ambient state is a *diff*, not a screen.**
`get_state`, `queue_update`, `compaction_*`, `agent_settled`, and (experimentally) replicated
snapshots give transitions, not frames. NAVI should model "idle / listening / thinking / blocked
on human / compacting / retrying / offline" as a small state machine fed by those events
(the header + `setStatus` + title + tray tooltip are all views of that one machine).
`StatusIndicatorKind` and the attachment/connection state unions are the vocabulary Pi already
uses. [inferred]

**A6. Persisted NAVI state belongs in custom session entries.**
`appendEntry` + `registerEntryRenderer` (`types.ts:1381,1358`) store durable data that survives
resume, is not sent to the LLM, and renders inline. Anything NAVI wants to remember about a
"signal" (source, timestamp, confidence, linked file/span) belongs there, **not** in the
conversation and not only in NAVI's own database — otherwise the session and the graph drift.
[inferred]

**A7. The look is cheap; the interaction is expensive.**
53 theme tokens + header/footer/widget factories + overlay geometry cover the entire aesthetic
brief (dark, cyan/blue, restrained, floating, minimal idle). What is *not* free is: who can
summon what, from where, with which context, and what happens to that request when the user
switches windows. Budget accordingly.

**A8. Aesthetics vs architecture, explicitly separated.**

| Purely aesthetic (optional, changeable without architecture) | Architectural (hard requirements) |
|---|---|
| dark cyan/blue palette; 53-token theme JSON | one long-lived agent session that outlives any window |
| CRT scanline/noise overlay, flicker, glyph choice | an explicit prompt-authority owner |
| floating-panel shadow/border style, `▚ ▞ ░` textures | a keybinding/gesture that summons the ask surface |
| "signal detected" wording and animation curve | a signal source (`ui_prompt_start`, `agent_settled`) wired to it |
| braille/pulse spinner frames (`setWorkingIndicator`) | a fan-out transport so N surfaces share one lane |
| header mascot / sigil | selection/context capture that is *typed data*, not prose |
| graph layout algorithm, node glyphs | durable non-LLM store for graph/radar records |
| voice waveform visualisation | text-in/text-out plumbing for the voice path |

---

## Recommended Approach

### R0. Shape of the system (one sentence)

**A NAVI daemon owns one Pi `AgentSession` in-process; it exposes one event/state stream to N
presentation surfaces; exactly one surface holds prompt authority at a time; the terminal surface
is built from Pi's own TUI primitives, and every other surface is built by NAVI from the event
stream.**

### R1. Topology (recommended)

```
                         ┌─────────────────────────────────────────────┐
                         │ NAVI daemon  (Node, single process)         │
                         │                                             │
  ResourceLoader ───────▶│ createAgentSession() / AgentSessionRuntime   │  docs/sdk.md
  (NAVI extensions)      │   · AgentSession  (the ONE session)         │
                         │   · event bus (subscribe)                   │
                         │   · sessionManager (tree, entries)          │
                         │   · appendEntry store for radar/graph       │
                         │   · local hub: WS/JSONL, multi-subscriber   │
                         └───┬───────────┬───────────┬─────────────┬───┘
                             │           │           │             │
              prompt authority │           │ read-only │  read-only  │ notifications
                             ▼           ▼           ▼             ▼
                  ┌───────────────┐ ┌───────────┐ ┌──────────┐ ┌──────────────┐
                  │ A. Terminal   │ │ B. Desktop│ │ C. Browser│ │ D. Tray/OS    │
                  │    surface    │ │   overlay │ │   panel   │ │   presence    │
                  │  (primary)    │ │ (alt-     │ │ (graph,   │ │ (state glyph, │
                  │  Pi TUI +     │ │  primary) │ │  radar)   │ │  toasts,      │
                  │  overlays     │ │           │ │           │ │  voice I/O)   │
                  └───────────────┘ └───────────┘ └──────────┘ └──────────────┘
```

Rationale, each leg tied to evidence:

- **In-process SDK, not RPC** — `docs/sdk.md` gives one process, full event fidelity, no JSONL
  framing traps (`docs/rpc.md:30-37`), no degraded `ExtensionUIContext` (`docs/rpc.md:1195-1203`),
  and direct access to `sessionManager.getTree()`/`exportToHtml()`.
- **NAVI owns the fan-out** — because base RPC is single-client and Pi's multi-attachment server is
  experimental (`§1.7`, `§A3`). NAVI's hub also becomes the natural place for the
  prompt-authority lock (§R4).
- **Terminal surface uses Pi's TUI primitives** — `createChatViewport` (`chat-viewport.ts:22`) for
  layout, `setHeader`/`setFooter`/`setWidget`/`setStatus`/`setTitle` for chrome, `ctx.ui.custom
  ({overlay:true})` for the palette and panels. This is the *only* surface that gets floating UI
  for free.
- **Browser surface is NAVI-rendered** — the browser gets the event stream and the session tree;
  it renders its own graph and radar. It never tries to run Pi components.
- **The `Terminal` seam is the escape hatch** — if NAVI wants one renderer for both terminal and
  desktop, implement `Terminal` (`packages/tui/src/terminal.ts:71`) over a websocket and drive
  `TuiAltScreen` directly, exactly as `VirtualTerminal` does in tests
  (`packages/tui/test/virtual-terminal.ts:11`). Keep this as a **stretch** path, not the plan: it
  inherits fullscreen mouse/OSC/hardware-cursor semantics that a GUI window does not have.

### R2. The interaction model: "Signal Console"

The one non-negotiable design idea: **the chat window is a *log*, not a *surface*.** The default
screen contains no transcript.

#### R2.1 Idle state (what the screen shows when nothing is happening)

Requirement level: layout is **CORE**; the specific glyphs are aesthetic.

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ◤ NAVI   ⟨ idle ⟩                     ◇ 3 signals   ▚ ctx 42%   14:02:11     │ ← setHeader
├──────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│                                                                              │
│              ▁▂▁▁▃▂▁▁▂▄▆▃▁▁▂▁▁▂▁▁  · last activity 14:01 · 4 turns            │ ← widgetAbove
│                                                                              │
│                                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ ⌁ context  src/graph/resolver.ts:88-140         ◎ listening · 1 selection    │ ← widgetBelow
│ > ▊                                                                          │ ← editor (min 3)
│ ^␣ summon   ^R radar   ^G graph   ^T trace   ^X copy                          │ ← setFooter
└──────────────────────────────────────────────────────────────────────────────┘
```

- **What "idle" means concretely:** `agent_settled` has fired, `getContextUsage().tokens` is
  known, no `queue_update` backlog, `ui_prompt_start` not active. Everything else is a variant.
- The transcript is **not rendered** until it is asked for. Mount it as a collapsed widget or
  behind `^G`/`/log`. `minimal-mode.ts:63-127` proves whole message categories can render as
  `Text("", 0, 0)`.
- Ambient presence beyond the terminal window (this is the actual "ambient computer" claim):
  - `ctx.ui.setTitle(...)` braille/pulse glyph while streaming and a static sigil when idle
    (`examples/extensions/titlebar-spinner.ts:41-58`) — **CORE**, 4 lines.
  - `agent_settled` → desktop toast via OSC 777/99/Windows-toast (`examples/extensions/notify.ts:43-52`)
    — **CORE**.
  - tray/OS presence — **NAVI-side**, not Pi (no tray in Pi, §1.8).

#### R2.2 Presence trigger ("signal detected")

Everything here is **CORE**; only the animation is optional.

| Event | Source (verified) | NAVI response |
|---|---|---|
| `ui_prompt_start {kind, title}` | `runner.ts:441-480` | **hard signal:** raise the attention overlay, tint header, toast, badge. `kind` distinguishes `select/confirm/input/editor/custom`. |
| `ui_prompt_end` | `runner.ts:460-472` | clear the signal; the answer already flowed back |
| `agent_settled` | `types.ts:741`, `docs/rpc.md:905` | "ready/idle" — the only correct done edge |
| `agent_start` / `turn_start` | `types.ts:730/764` | "listening → working" transition; start the pulse |
| `queue_update` | `docs/rpc.md:1057` | "you have queued input" indicator; make `clear_queue` reachable |
| `compaction_start/end`, `auto_retry_*` | `docs/rpc.md:1077,1109` | background maintenance, low-key (never modal) |
| `session_start {reason}` | `types.ts:564` | resumption banner ("resuming signal 2026-…") |
| connection/attachment `degraded`/`reconnecting` | `connection.ts:19-25` | "signal lost" state, not an error dialog |

Design rule: **only `ui_prompt_start` may steal focus.** Everything else is ambient. This keeps
the "computer intelligence" feeling rather than "app demanding attention".

#### R2.3 Asking without a chat window

**CORE** = summon key + palette + typed context payload.

1. **Summon key.** `pi.registerShortcut("ctrl+space", …)` (`types.ts:1320`) in a NAVI extension,
   *and* the same action bound in NAVI's own frontends. In-terminal this is an app shortcut; in a
   browser/desktop shell NAVI owns the keybinding layer entirely (§1.8). `onTerminalInput`
   (`types.ts:147`) is the fallback if a key must bypass the editor.
2. **Palette overlay.** `ctx.ui.custom(factory, { overlay: true, overlayOptions: { anchor:
   "bottom-center", width: "70%", maxHeight: "60%", visible: (w) => w >= 80 } })`.
   Implementation guidance from `overlay-test.ts` (Focusable + `CURSOR_MARKER` for the inline
   input) and `doom-overlay` (continuous re-render is fine).
3. **Context capture is data, not prose.** The palette composes a typed payload:

```
  ┌───────────────────────────────────────────────────────────────────┐
  │ ▸ ask NAVI about…                                                 │
  │ ┌───────────────────────────────────────────────────────────────┐ │
  │ │ what changed in this file's call graph?                       │ │
  │ └───────────────────────────────────────────────────────────────┘ │
  │ ⌁ context   src/graph/resolver.ts:88–140   (selection)     [⌫]    │
  │ ⌁ context   3 open buffers · 1 failing test                [⌫]    │
  │ ⌁ mode      ⌥ ask / ⌥ act / ⌥ watch                        [tab]  │
  │ ──────────────────────────────────────────────────────────────────│
  │ ⏎ send    ⇥ add context    ⌥↑ trace    esc dismiss                │
  └───────────────────────────────────────────────────────────────────┘
```

   The context block is what turns ambient into useful: file + span + nearby symbols + failing
   tests + recent diff, assembled **client-side** and injected as a single `prompt`.
   Evidence that this is expressible: `InputEventResult = { action: "transform", text, images }`
   (`types.ts:880-883`) lets an extension rewrite an input before it reaches the agent; RPC
   `prompt` accepts `images` (`docs/rpc.md:51-53`); `get_tree`/`get_entries` supply the session side
   of the context.
4. **Second-class ask paths** (LIKELY): typed `:` / `#` prefixes via
   `addAutocompleteProvider` (`types.ts:227`); "ask about selection" from the editor's current
   text via `getEditorText` (`:221`); a `question`-style tool so *the agent* can push a prompt at
   the user (`examples/extensions/question.ts`, rendered by NAVI's own overlay so it is not lost
   when the terminal is unfocused).

#### R2.4 How answers appear (overlay vs panel vs ambient annotation)

Decision rule, in priority order:

| Answer shape | Surface | Pi mechanism |
|---|---|---|
| ≤ ~15 lines, "what is this" | **right-anchored overlay panel**, editor keeps focus | `custom(factory, {overlay:true, overlayOptions:{anchor:"right-center", width:"40%", offsetX:-1}, onHandle: h => h.unfocus({target: editor})})` — `docs/tui.md:175-179` |
| long-form / needs reading with scrollback | **full custom() takeover** or the transcript widget expanded | `ctx.ui.custom` without `overlay`; `ScrollView(document,{follow:"end"})` (`chat-viewport.ts:23`) |
| 1-line status ("indexed 41 symbols") | **ambient annotation**: `setStatus` + widget line | `types.ts:150,172` |
| structured result (graph, citations, table) | **dedicated panel**, driven by `terminate: true` tool output | `structured-output.ts:11-40` + `registerEntryRenderer` (`types.ts:1358`) |
| "I need you" | **attention overlay** + toast | `ui_prompt_start` + `notify` (`types.ts:144`) |

```
   ┌──────────────── transcript (scrollback / frozen) ────────────┬────────────────────┐
   │ …prior turns…                                               │ ◤ SIGNAL detected  │
   │                                                             │ ─────────────────  │
   │                                                             │ resolver.ts:88–140 │
   │                                                             │  ↳ 4 changes       │
   │                                                             │  ↳ 2 callers       │
   │                                                             │  ↳ 1 test broken   │
   │                                                             │ ── reading ──      │
   │                                                             │ The call graph     │
   │                                                             │ shows the resolver │
   │                                                             │ is now reached by… │
   │                                                             │                    │
   │                                                             │ ⇥ trace   ✎ graph  │
   │                                                             │ esc dismiss        │
   └─────────────────────────────────────────────────────────────┴────────────────────┘
        editor keeps focus; overlay is visible but unfocused (handle.unfocus({target:editor}))
```

#### R2.5 Research progress and graphs

- **Progress**: the status machine of §A5, rendered three ways at once — header glyph, footer
  status entry, tray tooltip / title. Sources: `StatusIndicatorKind`
  (`status-indicator.ts:7`) mirrored onto `agent_start/agent_settled/compaction_*/auto_retry_*/queue_update`.
  `setWorkingIndicator({frames, intervalMs})` (`types.ts:118,166`, `docs/tui.md:783-806`) is the
  animation hook.
- **Session/tab graph**: this is the one graph Pi hands you directly — `get_tree` returns
  `{entry, children, label, labelTimestamp}` (`docs/rpc.md:747-773`), and labels are settable via
  `pi.setLabel(entryId, label)` (`types.ts:1394`). Render it as the "wired" view: nodes = turns,
  edges = parentId, labels = user/navi beacons, collapsed branches = abandoned signals.
  `get_entries({since})` (`docs/rpc.md:719`) gives cheap incremental updates; `sm.getTree()`
  (`docs/rpc.md` / `session-format.md`) for SDK-side.
- **Research graph (files, symbols, claims)**: **not** a Pi concept. It must live in NAVI and be
  *anchored* to Pi state by storing each node as a custom entry
  (`appendEntry("navi.node", {kind, ref, span, evidence, ts})`) rendered by
  `registerEntryRenderer("navi.node", …)`. That gives durability across resume, non-pollution of
  context, and inline appearance in the transcript when the user wants the log.
- **Radar**: a "what is within reach right now" strip — open buffers, current selection, dirty
  files, failing tests, recent tool targets (`ToolRenderContext.args`/`cwd`, `types.ts:421-446`).
  Render as a widget-above line or a small `top-right` overlay toggled by `^R`; hide on narrow
  terminals with `overlayOptions.visible` (`docs/tui.md:161`).

#### R2.6 Multiple surfaces, one session, no duplicated state

Rules (all **CORE**):

1. **One owner of the session.** The NAVI daemon holds the single `AgentSession`; surfaces hold
   *views*. No surface keeps its own message array. This mirrors `client-tui.ts:501-505`, which
   renders only what the replicated snapshot says.
2. **Surface = (role, capability set).** Declare per surface:
   - `author` (may `prompt`/`steer`/`followUp`/`abort`/`clear_queue`) — exactly one at a time;
   - `viewer` (subscribes to transcript + status; may *propose* a prompt, which the author confirms);
   - `notifier` (receives state transitions only).
3. **Handoff is explicit.** Moving author role (terminal → browser → voice) must be a visible
   event in the same "signal" language, and must be serialized against in-flight turns:
   `steer` is delivered after the current assistant turn's tool calls, `followUp` only when the
   agent stops (`docs/rpc.md:56-63`) — so a handoff mid-turn must choose one of those two, and
   NAVI should surface which.
4. **Cursor semantics from Pi, reused.** `get_entries({since: lastSeenId})` (`docs/rpc.md:719`)
   is a durable cursor that survives client restarts; a viewer that reconnects resumes from its
   last id instead of re-fetching everything. **This is the anti-duplication mechanism.**
5. **Replicated-state discipline (if adopting Pi's server).** `transcript.ts:4-15` publishes a whole
   coherent snapshot; consumers hold immutable copies (`apply(snapshot)`), never deltas
   (`client-tui-chat.ts:40-56`). If NAVI writes its own hub, copy this contract: publish whole
   snapshots of small tracked state, let clients diff for rendering.

#### R2.7 What belongs in the browser vs a desktop shell

| Concern | Browser panel | Desktop shell |
|---|---|---|
| graph/radar/timeline rendering, dense typography, animation | **yes** — `[external]` best canvas/typography stack | only as an embedded webview |
| keyboard capture when the user is in another app | **no** (unless a browser extension/host permission — NAVI must supply the host) | **yes** — OS-level global hotkey is NAVI's, absent from Pi (§1.8) |
| ambient overlay above other apps, tray, toast | **no** | **yes** |
| microphone / voice I/O | partial (browser `getUserMedia`, with permission UX) | **yes** |
| text rendering of Pi's own TUI | no | yes, via a terminal host or an embedded `Terminal` sink |
| session export/snapshot viewing | **yes** — this is literally what `/share` produces (`session-share.ts:72,108`) | yes |
| "answer without a chat window" | yes — panel that appears on `ui_prompt_start` | yes — overlay |
| long-lived presence without a window | **no** | **yes** |

Rule of thumb: **browser = perception and reading; desktop shell = presence, capture, and
notification.** The agent itself lives in neither.

#### R2.8 Wireframe: the chosen model, end to end

```
IDLE                                             SIGNAL DETECTED (ui_prompt_start)
┌──────────────────────────────────┐            ┌──────────────────────────────────┐
│ ◤ NAVI ⟨idle⟩        ◇3  ▚42%   │            │ ◤ NAVI ⟨blocked⟩ ◉   ◇3  ▚44%   │
├──────────────────────────────────┤            ├──────────────────────────────────┤
│                                  │            │ ╔══════════════════════════════╗ │
│         ▁▂▁▃▂▁▁▂▄▆▃▁▁▂▁           │            │ ║ ◤ SIGNAL — needs you         ║ │
│                                  │            │ ║ "Allow write to .env?"       ║ │
├──────────────────────────────────┤            │ ║ [ allow ] [ block ] [ diff ] ║ │
│ ◎ listening · superset 14 / 22   │            │ ╚══════════════════════════════╝ │
│ > ▊                              │            ├──────────────────────────────────┤
│ ^␣ summon  ^R radar  ^G graph    │            │ > ▊                              │
└──────────────────────────────────┘            │ ^␣ summon  ^R radar  ^G graph    │
                                                 └──────────────────────────────────┘

ASKING (^␣)                                      ANSWERING (overlay, unfocused)
┌──────────────────────────────────┐            ┌──────────────────┬───────────────┐
│ ◤ NAVI ⟨idle⟩        ◇3  ▚42%   │            │ …transcript…     │ ◤ SIGNAL      │
├──────────────────────────────────┤            │                  │ res→graph     │
│  ╔══════════════════════════╗    │            │                  │ ↳4 ↳2 ↳1 test │
│  ║ ▸ ask NAVI about…        ║    │            │                  │ ── reading ── │
│  ║ ask about selection…     ║    │            │                  │ The resolver  │
│  ║ ⌁ resolver.ts:88–140     ║    │            │                  │ is now reac…  │
│  ║ ⏎ send   esc dismiss     ║    │            │                  │ ⇥trace ✎graph │
│  ╚══════════════════════════╝    │            │                  │ esc dismiss   │
├──────────────────────────────────┤            ├──────────────────┴───────────────┤
│ > ▊                    (focused) │            │ > ▊              (still focused) │
└──────────────────────────────────┘            └──────────────────────────────────┘

GRAPH (^G)                                       RADAR (^R) / multi-surface
┌──────────────────────────────────┐            ┌──────────────────────────────────┐
│ ◤ NAVI ⟨idle⟩                    │            │ ◤ NAVI ⟨browser: viewer⟩         │
├──────────────────────────────────┤            ├──────────────────────────────────┤
│   ╭─╮                            │            │  terminal ● author               │
│   │▣│ turn 1 · labeled "premise"  │            │  browser  ○ viewer  ⇥ 3 nodes    │
│   ╰┬╯                            │            │  tray     ○ notifier             │
│  ╭─┴─╮   ╭───╮                   │            │  voice    ○ standby              │
│  │▣▣│─┬─│▣▣▣│  ⛌ abandoned branch │            │ ──────────────────────────────── │
│  ╰──╯ │ ╰───╯                   │            │  ◎ listening          [⌥ handoff] │
│       ╰─ turn 4 · navi.node      │            └──────────────────────────────────┘
│ ⇥ jump  ⏎ open  ctrl+o filter     │
└──────────────────────────────────┘
```

### R3. Concrete mapping: NAVI surface → Pi mechanism

| NAVI element | Pi mechanism (verified) | Level |
|---|---|---|
| idle header/state glyph | `setHeader` (`types.ts:192`, `custom-header.ts`) | CORE |
| ambient status line | `setStatus` + `setWidget(aboveEditor)` (`types.ts:150,172`) | CORE |
| affordance hints | `setFooter` (`types.ts:185`, `custom-footer.ts`) | CORE |
| background presence in OS title | `setTitle` (`types.ts:195`, `titlebar-spinner.ts`) | CORE |
| desktop toast on ready | `agent_settled` + OSC 777/99 / Windows toast (`notify.ts`) | CORE |
| summon key | `registerShortcut` (`types.ts:1320`); `onTerminalInput` (`:147`) | CORE |
| command surface | `custom({overlay:true, overlayOptions})` (`types.ts:198`, `docs/tui.md:126-172`) | CORE |
| answer panel that coexists with typing | overlay `handle.unfocus({target: editor})` (`docs/tui.md:175-179`) | CORE |
| answer as data | `defineTool({terminate:true})` (`structured-output.ts:11-40`) | CORE |
| durable signal records | `appendEntry` + `registerEntryRenderer` (`types.ts:1381,1358`) | CORE |
| graph overlay | `get_tree` (`docs/rpc.md:747`) / `sessionManager.getTree()` + `custom()` | CORE |
| input rewriting / context injection | `InputEventResult{transform}` (`types.ts:880-883`) | CORE |
| "you have queued input" | `queue_update` + `clear_queue` (`docs/rpc.md:1057,158`) | CORE |
| palette prefix triggers | `addAutocompleteProvider` (`types.ts:227`) | LIKELY |
| tool-density calm mode | `setToolsExpanded` / custom renderers (`minimal-mode.ts`) | LIKELY |
| NAVI as installable plugin | Chord presentation facets (`chord/README.md:124-178`, `plugins/bundled.ts`) | EXPERIMENTAL |
| N surfaces via Pi's own server | `startServer` + multi-`attachmentId` (`server.ts:521`, `session-worker-manager.ts:205-239`) | EXPERIMENTAL |
| full Pi TUI inside a desktop/browser sink | `Terminal` seam + `TuiAltScreen` (`tui/src/terminal.ts:71`, `virtual-terminal.ts:11`) | EXPERIMENTAL |
| session viewer in browser | `session.exportToHtml` (`agent-session.ts:3463`), `/share` viewer | LIKELY (read-only) |

---

## Alternatives

**Alt-1. RPC subprocess + NAVI hub.** `pi --mode rpc` as a child process; NAVI's daemon is the sole
stdin writer and fans stdout JSONL out to N surfaces.
*Pros:* process isolation, language-agnostic, survives agent crashes, no SDK coupling,
`id`-correlated responses (`docs/rpc.md:26`).
*Cons:* extension UI is degraded to 5 dialogs (`docs/rpc.md:1195-1203`) — **NAVI loses `setFooter`,
`setHeader`, `setWidget` factories, `custom()`, and `setTheme` on the agent side**; JSONL framing
must be hand-rolled correctly (`:30-37` — `readline` is non-compliant); `message_update` has no
cumulative message so NAVI must assemble partials (`:991-996`).
*Verdict:* the right fallback if the daemon must be polyglot. Not the default.

**Alt-2. Terminal-only NAVI (no browser/desktop).** All surfaces are Pi TUI constructs.
*Pros:* 100 % of `ExtensionUIContext` available; overlays, mouse, images, theme tokens free; no
new rendering stack; fastest to a convincing prototype.
*Cons:* no presence when unfocused except title/toast; mouse only in fullscreen mode
(`docs/tui.md:326`); "browser panel" is impossible.
*Verdict:* **the right phase 1.** It is a strict subset of R1 and validates the interaction model
before any GUI exists.

**Alt-3. Adopt Pi's experimental multi-attachment server.** `startServer()`
(`server.ts:521`) + session workers; NAVI surfaces are extra attachments.
*Pros:* N surfaces to one session with per-attachment scope, coherent replicated state, reconnect
and degraded states already modelled (`connection.ts:19-25`), Chord presentation plugins
(`chord/README.md:10-27`).
*Cons:* everything here is `src/experimental/*`; `ServerLifetime` retires the server ~1 s after the
last detach unless `keepAlive` (`server.ts:244-245,258-260`); plugins use a bespoke bundling and
`node:vm` loading path (`chord/README.md:124-192`); the only consumer is one experimental client.
*Verdict:* reconsider after Pi promotes it. Meanwhile, copy its *contracts* (snapshot replication,
attachment state machine) into NAVI's own hub.

**Alt-4. Custom `Terminal` over a socket; render Pi's real TUI anywhere.**
`createInteractiveTui({ terminal })` (`tui-renderer.ts:12,22`) + an xterm.js-backed `Terminal`
implementing `write/moveBy/clear*/setTitle` (`tui/src/terminal.ts:71-112`).
*Pros:* literally reuses every Pi component, overlay, and theme; one renderer for terminal and
desktop; `VirtualTerminal` proves the seam (`tui/test/virtual-terminal.ts:11`).
*Cons:* the stock `InteractiveMode` does not accept a `terminal` (`interactive-mode.ts:352-373`),
so NAVI must compose its own `ViewportTUI` (as `client-tui.ts:743-785` does) or patch; the browser
then inherits terminal semantics (cell grid, no proportional text) — which contradicts
"information graphs / floating elements" ambitions.
*Verdict:* keep for the desktop-overlay surface specifically (a terminal is acceptable there);
do not use it for the browser panel.

**Alt-5. NAVI as a Pi plugin package (facet bundle).** Ship NAVI's UI as a Chord presentation
facet so it loads per session (`plugins/bundled.ts:32-45`, `docs/packages.md`).
*Pros:* distribution and hot-swap come free; matches Pi's own extensibility story.
*Cons:* depends on experimental plumbing; facet bundles are content-addressed, SHA-256-verified,
host-resolved — meaningful build complexity for a project that also needs its own daemon.
*Verdict:* packaging strategy, not architecture. Revisit once the UI is stable.

---

## Risks

1. **The RPC cliff.** If NAVI's non-terminal surfaces are built on RPC, the *agent-side* extension
   is crippled (`custom()` → `undefined`, `setFooter/setHeader/setWidget(factory)` → no-ops,
   `setTheme` → failure; `docs/rpc.md:1195-1203`). Any NAVI feature that assumes "the agent can
   raise its own panel" silently works in the terminal surface and silently does nothing
   elsewhere. **Mitigation:** route all agent-side UI through NAVI's daemon (SDK path) or make
   every agent-side UI call mode-checked (`ctx.mode === "tui"`), as every example does
   (`qna.ts:30`, `question.ts:54`, `doom-overlay/index.ts:26`).
2. **Daemon lifetime / background presence.** Pi's server retires on zero connections+workers with
   a 1 s grace (`server.ts:244-245,299-320`). A tray-only NAVI with no window will look alive and
   be dead. **Mitigation:** hold a keepalive connection, or own the process and never let the SDK
   session go idle-disposed.
3. **Prompt-authority ambiguity.** Two surfaces can prompt concurrently; `steer`/`followUp`
   semantics are per-turn (`docs/rpc.md:56-63`). **Mitigation:** explicit role lock (§R2.6) and
   refusing `prompt` from non-author surfaces with a visible reason.
4. **Overlay lifecycle footgun.** Overlay components are disposed on close; re-showing requires a
   fresh `custom()` call (`docs/tui.md:183-200`). Long-lived NAVI panels that hold references will
   render into disposed components. **Mitigation:** keep state in NAVI's store, treat the overlay
   as a pure projection, re-`custom()` on every show.
5. **Focus theft.** `ui_prompt_start` is the only signal that should steal focus. Over-eager
   attention moves (every tool call, every token) destroy the ambient feeling and make NAVI
   worse than a chat window. **Mitigation:** the §R2.2 table *is* the whitelist.
6. **Component duplication in two frontends.** Terminal surface uses Pi components; browser uses
   NAVI components. Two implementations of "the answer panel" will drift. **Mitigation:** define
   one NAVI-side *view model* (a typed "signal" and "response" record) and two renderers — never
   two state models.
7. **Event-shape churn.** `message_update` deliberately omits the cumulative message
   (`docs/rpc.md:991-996`); `agent_settled` vs `agent_end` are easy to confuse (`:895-911`);
   experimental services may change. **Mitigation:** wrap Pi events behind one adapter module;
   never let UI code read raw RPC events.
8. **Fullscreen-only affordances.** Mouse regions, clickable links, drag-select exist only in
   fullscreen/alt-screen mode (`docs/tui.md:312-326`); regular mode gives the terminal its
   scrollback. A NAVI design with clickable graph nodes forces fullscreen, which changes the
   user's scrollback behaviour. **Mitigation:** treat pointer affordances as progressive
   enhancement with keyboard equivalents.
9. **Theme completeness churn.** 53 required tokens (`docs/themes.md`); a NAVI theme missing a
   token fails to load. **Mitigation:** generate the theme from a NAVI palette definition.
10. **Voice has no Pi story at all.** No audio anywhere in the repo (§1.8). Voice is a NAVI
    surface that must terminate in a *text* `prompt`/`steer` call, and its audio never becomes
    session state unless NAVI transcribes it into a custom entry. Risk: voice appears cheap and is
    not (permissions, latency, false triggers, hallucinated transcripts).
11. **Adopting experimental surfaces too early.** `src/experimental/*` includes the whole
    server/coordinator/worker/plugin stack; Radii relay, Chord bundling, and `node:vm` facet
    loading are all in that blast radius.
12. **The "chat window" gravity.** Every convenience (transcript always visible, editor always at
    the bottom, answers inline) pulls NAVI back toward being a chat app. The ambient claim rests
    entirely on idle-state minimalism and on the signal/palette path being *faster* than typing a
    message. If the palette is slower, the transcript wins.

---

## Feature Assessment

Format: **feature — level — rationale (evidence)**.

**CORE (architecture; without these NAVI is a chat window)**

- Single long-lived agent session owned by a NAVI daemon — SDK `createAgentSession`/`AgentSessionRuntime` is the only path retaining full event + UI fidelity (`docs/sdk.md`; `docs/rpc.md:1195-1203`).
- One event/state stream fanned out to N surfaces — base RPC is single-client (`docs/rpc.md:20-26`); NAVI must own the fan-out.
- Explicit prompt authority held by exactly one surface at a time — concurrent prompters are already possible via `AgentController` (`client-tui.ts:563-585`).
- Idle state with no transcript — supported today by custom renderers returning empty components (`minimal-mode.ts:63-127`) and by widget/header slots.
- Summon keybinding registered through Pi — `registerShortcut` (`types.ts:1320`), dispatched via `onExtensionShortcut` (`interactive-mode.ts:2072`).
- Floating ask-surface — `custom({overlay:true, overlayOptions})` with anchors/percentages/`visible` (`types.ts:198-212`, `docs/tui.md:126-172`).
- Answer panel that coexists with typing — overlay focus control `handle.unfocus({target})` (`docs/tui.md:175-179`).
- `ui_prompt_start`/`ui_prompt_end` wired as the signal-detected trigger — `runner.ts:441-480`, kind+title.
- `agent_settled` (not `agent_end`) as the ready edge — `types.ts:741`, `docs/rpc.md:905-911`, `notify.ts:43-45`.
- Typed context capture injected as one prompt — `InputEventResult{transform}` (`types.ts:880-883`), RPC `prompt` with images (`docs/rpc.md:51-53`).
- Durable, non-LLM signal/graph records — `appendEntry` + `registerEntryRenderer` (`types.ts:1381,1358`, `entry-renderer.ts:19-38`).
- Ambiguity/attention hygiene: only human-blocking signals may steal focus — derived from `withUIPrompt` being the sole wrapped path (`runner.ts:444-449`).
- Ambient status as a single state machine, rendered in ≥3 places — `StatusIndicatorKind` (`status-indicator.ts:7`) + `setStatus`/`setTitle`/footer (`status-line.ts`, `titlebar-spinner.ts`, `custom-footer.ts`).
- Dark cyan/blue restrained look as a theme JSON — 53 tokens + `vars` + hot reload (`docs/themes.md`).
- Desktop notification on ready — OSC 777/99 + Windows toast (`notify.ts:11-52`).
- Incremental resync by durable cursor — `get_entries{since}` (`docs/rpc.md:719`), `leafId` in one round trip.
- Reconnect/degraded UX vocabulary — `connecting|connected|disconnected`, `detached|attaching|attached|degraded` (`connection.ts:19-25`).

**LIKELY (strong fit, modest risk)**

- Session tree rendered as a "wired" graph overlay — `get_tree` gives `{entry, children, label}` directly (`docs/rpc.md:747-773`); labels writable via `setLabel` (`types.ts:1394`).
- Research graph anchored to session entries and drawn in NAVI — needs a NAVI store; Pi supplies only the anchor (`appendEntry`).
- Context radar strip — assembled client-side from `cwd`, selection, open buffers, `ToolRenderContext` (`types.ts:421-446`).
- Prefix/trigger autocompletion in the palette (`:` `#`) — `addAutocompleteProvider` (`types.ts:227`).
- Calm/verbose density toggle — `setToolsExpanded` (`types.ts:283`) + custom tool renderers.
- Read-only browser view of a session — `exportToHtml` (`agent-session.ts:3463`), `/share` viewer (`session-share.ts:72,108,191`).
- Voice as a text-terminal surface — no audio in Pi (§1.8); speech must reduce to `prompt`/`steer` (`docs/sdk.md`).
- Custom editor as a prompt-console (modal feel) — `setEditorComponent` (`types.ts:262`, `modal-editor.ts`).
- Tool-call "trace" streaming view — `tool_execution_update` accumulated `partialResult` (`docs/rpc.md:1055`), `message_update` deltas (`:961-996`).

**EXPERIMENTAL (real in-tree, unstable; adopt only behind an adapter)**

- N surfaces via Pi's own server (`startServer`, `session-worker-manager.ts:205-239`) instead of a NAVI hub.
- Chord presentation facets / plugin packages for shipping NAVI UI (`chord/README.md:124-192`, `plugins/bundled.ts:32-45`).
- Injecting a custom `Terminal` so Pi's TUI renders into a non-TTY sink (`tui/src/terminal.ts:71`, `tui/test/virtual-terminal.ts:11`) — needs composing a `ViewportTUI` yourself (`tui.ts:456-458`) because `InteractiveModeOptions` has no `terminal` (`interactive-mode.ts:352-373`).
- Radius relay as a remote transport for surfaces (`client-runtime.ts:126-133`, `radius-auth.ts`).

**DEFER (real value, but only after phase-1 validation)**

- OS-global hotkey and system tray presence — **nothing in Pi** (§1.8); a desktop-shell project of its own.
- Browser extension / tab-graph across pages — requires a NAVI browser host; Pi has no browser surface.
- Multi-turn "watch mode" (agent observes ambient signals without being asked) — needs the signal taxonomy in production first.
- Per-surface permission/consent model beyond the author/viewer/notifier split.
- Session-share-like browser viewer as an interactive (not exported) surface.
- Voice output (TTS) of responses.

**REJECT (do not do; each has an in-tree reason)**

- Reimplementing Pi's TUI components for the browser — components are ANSI string producers (`docs/tui.md:14-31`); use the `Terminal` seam or NAVI-native components instead.
- Forking/patching `InteractiveMode` to add surfaces — it is a single-mode class bound to `ProcessTerminal` via `createInteractiveTui` (`interactive-mode.ts:534,819`); compose your own viewport as `client-tui.ts:743-785` does.
- Keeping NAVI state (radar, graph, annotations) inside the LLM context — `appendEntry` exists precisely so it is not sent to the LLM (`types.ts:1381`).
- Using `agent_end` as "done" — retries, compaction, and queued continuations still follow (`docs/rpc.md:895-911`; `notify.ts:43-45` says so explicitly).
- Treating `notify` as an attention signal — it is not wrapped by `withUIPrompt` (`runner.ts:442-450`), so no `ui_prompt_*` event accompanies it.
- Making the browser the primary prompt surface — prompt authority + context capture + tools are closest to the terminal (mode matrix, `docs/extensions.md:2940-2947`).
- Pop-up modals for background work (compaction, retry, indexing) — `StatusIndicatorKind` already classifies them as ambient (`status-indicator.ts:7`).
- Depending on RPC-mode `custom()`/`setFooter`/`setTheme` — explicitly no-ops/failing (`docs/rpc.md:1195-1203`).
- Designing for mouse-first interaction — mouse capture exists only in fullscreen mode (`docs/tui.md:326`), and regular mode hands scrollback to the terminal.

---

## Open Questions

1. **Session ownership boundary.** Does NAVI own the `AgentSession` in-process (SDK) permanently,
   or delegate to a Pi `pi server` process? The answer decides whether NAVI's agent-side extension
   can ship floating UI at all (`docs/rpc.md:1195-1203`). *Needs a decision before any UI work.*
2. **Does "ambient" mean always-running?** If NAVI must act without a user prompt (watch mode), the
   whole interaction model changes (budgets, consent, idle-state semantics beyond `agent_settled`).
   Pi gives no scheduling primitive; the daemon must own it.
3. **Is the terminal surface primary forever, or is the desktop overlay the product?** Alt-2 vs
   Alt-1/Alt-4. Terminal-first is a strict subset and much cheaper; desktop-first requires a
   rendering stack and an OS integration layer Pi does not provide.
4. **What exactly is a "signal"?** NAVI's vocabulary needs a formal definition (source event,
   ttl, confidence, anchor, action). `ui_prompt_start` is the only human-blocking one; everything
   else is a status transition. Without this the "signal detected" aesthetic becomes decoration.
5. **Prompt-authority protocol.** Timeout-based handoff? Explicit take/give? What happens to a
   `steer` queued by a surface that loses authority mid-turn (`docs/rpc.md:56-63`)?
6. **Browser panel transport.** NAVI's own WS hub, or the Chord facet/RPC boundary
   (`chord/README.md:46-79`)? The latter is more portable and much less mature.
7. **How much of Pi's session tree is user-visible?** Full branch graph vs a linearized
   "signal log". `get_tree` supports the former; the ambient brief may argue for the latter as the
   default with the tree as an on-demand overlay.
8. **Voice scope.** Push-to-talk only, or always-listening? Always-listening has no Pi mechanism
   and no consent model in-repo; push-to-talk reduces to a text surface (§R3) and is far cheaper.
9. **Where does NAVI's research graph get its non-session facts?** Pi stores evidence per entry;
   cross-session claims need a NAVI store with its own persistence and a defined relationship to
   session files (`docs/session-format.md`).
10. **Do we adopt fullscreen mode?** It enables mouse and clickable overlays but takes over the
    scrollback and requires keybinding re-education (`docs/keybindings.md:89-108`,
    `docs/settings.md:69-72`). This is a user-visible product decision, not a technical one.
11. **Theme coupling.** Does NAVI ship a first-class theme (53 tokens) plus export colors, or
    restyle only its own surfaces and inherit `dark`? The former is required for an
    HTML/`/share` output to look like NAVI (`docs/themes.md` export section).
12. **Upgrade policy against `src/experimental/*`.** If NAVI depends on Chord or the experimental
    server, what is the contract for Pi upgrades? Currently none exists in-repo.
