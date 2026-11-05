# Section 2 — 3D stage and layout

Scope: `ui/scene.js`, `ui/copland.js`, `ui/surface.css`, `ui/station.css`, `ui/index.html`, `ui/station.js` (scene calls), `window.py` (sizing).

**Measured facts.** At 1440×920 the terminal occupies 12–74 vh and the chat 61–97 vh: a 119.6 px overlap. At 640 px height the chat grows to 42 vh and the overlap is 115.2 px. The emblem spans roughly 36–66 % of viewport height and the floor band 66–80 %, so both are covered. `setLayout` exists and is called exactly once with `(.5, 0)` (`ui/station.js:390`), and `scene.js:170` clamps the horizontal position at `.5`, so the stage cannot move right. `scene.bundle.js` is what `index.html` loads; scene edits require `npm run bundle`.

**Layout contract used by this section (L0).** One fixed `#shell` grid with areas `strip / stage / graph / composer`, `gap:12px`; the canvas remains full-bleed behind it. `#stage-region` is an empty measurement cell; the scene is driven by its rect, not by percentages. `--graph` and `--dock` columns are `0px` until enabled. `#field` keeps `position:fixed;inset:0`, so the backdrop stays full-bleed while the *framing* follows the stage cell. Every plan below assumes L0 unless stated.

---

## BUG-020 · The stage is always dead-centre behind the panels
- **Severity** Critical · **Type** layout/scene API
- **Evidence** `ui/station.js:390` (`setLayout(.5,0)` — the only call); `ui/scene.js:271`; `ui/scene.js:170` (cap at `.5`); `ui/station.css:4-6,71-72,78-79` (three independently anchored surfaces); `ui/index.html:13-24`.
- **Root cause** A framing API exists but is never driven, and the panels are positioned without reference to the stage. The clamp at `.5` also makes right-of-centre unrepresentable.
- **Fix** (1) Implement L0: grid shell, `#stage-region`, CSS variables `--graph`/`--dock`, breakpoints including 640 px height. (2) Replace percentage anchors: chat `grid-area:composer`, sessions `strip`, terminal `stage` with a dock column, graph `graph`. (3) Extend `inspect()` with `layout:{stage,chat,terminal,sessions,graph,overlaps}` from `getBoundingClientRect()`. (4) Drive the scene from the stage rect: `naviScene.setFrame({x,y,w,h}, dim)`; generalise `frame()` to `fullW = 2*W*max(p,1-p)`, `fullH = 2*H*max(q,1-q)`, offsets `fullW/2 − p*W`, `fullH/2 − q*H`, `p,q ∈ [.08,.92]`; delete the `.5` cap and the unimplemented "left third" comment. (5) Delete `transition:opacity .6s` on `#field` and write opacity only when the rounded value changes, or move dimming into the composer exposure (absorbs the former S2-33/S2-34).
- **End state** The emblem has an unobstructed cell; opening the graph panel shifts the stage instead of covering it; `inspect().layout.overlaps` is empty at every supported size.
- **Why** The 3D stage is the product's identity, and every panel currently sits on top of it.
- **Depends** none (lands first) · **Verify** V-PNG at 1440×920 and 960×640: assert zero overlaps and an emblem bounding box inside the stage cell; `layout` numbers computed, not eyeballed.
- **Effort** L · **Risk** medium: the bundle must be rebuilt and every capture regenerated.

## BUG-021 · The chat panel covers the emblem's lower third
- **Severity** High · **Type** layout
- **Evidence** `ui/station.css:4-6` (`bottom:3vh; max-height:36vh`), `:7` (`min-height:72px` — the panel grows into the cap), `:57` (`42vh` under 720 px); emblem 36–66 %, chat top at 61 %.
- **Root cause** Percentage sizing with no relationship to the stage, plus a small-window rule that enlarges the panel exactly when space shrinks.
- **Fix** After L0: composer row is `minmax(0, auto)` with `max-height: min(30vh, 320px)` and `min-height: 120px`; the log scrolls inside it. Remove the 42 vh override; at short heights collapse the composer to one line plus a "history" toggle.
- **End state** The chat sits below the stage cell at every size; the emblem base and dots are visible.
- **Why** The first thing a user notices is that the art is cut in half.
- **Depends** BUG-020 · **Verify** V-PNG: emblem bottom above the composer top; no clipping at 640 px.
- **Effort** S · **Risk** low.

## BUG-022 · The floor glow is invisible
- **Severity** Medium · **Type** rendering/layout
- **Evidence** `ui/scene.js:77-92` (radius 1.75, `rotation.x=−π/2`, `y=−1.5`, alpha ≈0.93); camera elevation ≈15° (`:198-199`); the band projects to 66–80 % and sits under the chat.
- **Root cause** It is edge-on, covered, and its dark-blue inner "lift" (0.55 weight) never reaches the bloom threshold.
- **Fix** With L0 freeing the cell: raise camera elevation slightly (or tilt the plane a few degrees), move the plane to the stage cell's lower third, raise the inner colour above the bloom threshold, and reduce alpha to ~0.75 so it reads as a glow rather than a slab.
- **End state** A visible elliptical glow anchors the emblem inside the stage cell.
- **Why** The floor is the composition's ground plane; without it the emblem floats.
- **Depends** BUG-020 · **Verify** V-PNG: sample pixels along the ellipse; luminance above the backdrop and no cut by a panel edge.
- **Effort** S · **Risk** low.

## BUG-023 · The halo is sliced by the panel edge
- **Severity** Medium · **Type** rendering
- **Evidence** `ui/scene.js:74` (sprite scale 4.2 → ±1.68 world); `:195` (fadeables); `:196` (opacity); floor at `z≈0` vs halo at `z=−.6` causes a hard elliptical terminator through the lower half.
- **Root cause** Transparent sorting plus an overlay boundary; the halo was sized for a full-window composition.
- **Fix** Parent the halo to the stage cell, reduce scale to ~3.2, set `depthWrite:false` and an explicit `renderOrder` above the floor, and let L0's dim channel fade it when panels are open.
- **End state** A soft, complete halo around the emblem, never cut by a rectangle.
- **Why** A hard cut through a glow is the clearest "unfinished" signal in a 3D scene.
- **Depends** BUG-020 · **Verify** V-PNG: no straight-edge luminance discontinuity in the halo band.
- **Effort** S · **Risk** low.

## BUG-024 · The terminal pane overlaps the chat by ~120 px
- **Severity** High · **Type** layout
- **Evidence** `ui/station.css:78-79` (`top:12vh; height:min(62vh,620px)`), `:4-5` (`bottom:3vh; max-height:36vh`); both centred with widths 960 vs 920. Measured 119.6 px at 920 px height.
- **Root cause** Two independently anchored fixed boxes, both horizontally centred.
- **Fix** L0 dock: the terminal occupies the stage row as a second column (`--dock: min(46vw, 720px)`) or a bottom row (`data-dock="bottom"`), never overlapping the composer. Add a drag splitter and persist the ratio; the host already accepts resize messages (`window.py:104-107`).
- **End state** Terminal and chat never intersect; the split is adjustable and remembered.
- **Why** The overlap is the single most visible layout defect today and makes both surfaces feel broken.
- **Depends** BUG-020 · **Verify** V-FLOW: open the terminal, assert `intersection(chat, terminal)` is empty at 1440×920, 1280×720, 960×640; splitter drag changes both rects.
- **Effort** M · **Risk** medium (xterm fitting already exists; hook it to the splitter).

## BUG-025 · Small windows make the layout worse
- **Severity** High · **Type** layout/host
- **Evidence** `window.py:44-47` uses `max(960,…)`/`max(640,…)` defaults with no size request, so the window can shrink below them; `ui/scene.js:182-183` shrinks the stage under 1100×700; `ui/station.css:57` grows the chat under 720 px; terminal keeps 62 vh.
- **Root cause** Every metric is a viewport percentage and nothing has a minimum size contract.
- **Fix** L0 breakpoints: `>=1200` three columns; `900–1199` dock under the stage; `<900` stack (sessions strip, stage, terminal tab, composer) with the graph as a drawer; enforce `set_size_request(720, 560)` in `window.py`; replace the 42 vh rule.
- **End state** Usable at 960×640: stage visible, chat two lines minimum, terminal toggled rather than overlapping.
- **Why** Laptop and tiled-window use is the common case.
- **Depends** BUG-020, BUG-024 · **Verify** V-SIZE at 960×640 and 1280×720: zero overlaps; composer ≥120 px; stage ≥180 px tall.
- **Effort** M · **Risk** medium: breakpoint QA needs the new `--size` harness flag.

## BUG-026 · Panels are translucent and blur the artwork
- **Severity** Medium · **Type** rendering/perf/a11y
- **Evidence** `ui/station.css:1` (`.glass`: `rgba(.82/.86)` gradient + `backdrop-filter:blur(14px)`), applied to chat, sessions, terminal, dialogs, toast.
- **Root cause** One glass style used over a bright animated backdrop: text contrast varies with the animation, and WebKit re-blurs four regions every frame (dominant cost under software GL).
- **Fix** Introduce two surfaces: `--surface-panel` (opaque enough for 7:1 text contrast, no backdrop filter) and `--surface-overlay` (glass, only for dialogs/short-lived toasts). Remove `backdrop-filter` from persistent panels; keep blur only where the scene should show through.
- **End state** Text contrast is stable and readable; software rendering gets faster.
- **Why** Blur over motion is both a legibility and a performance problem.
- **Depends** BUG-020 · **Verify** V-PNG: compute contrast of `--ice` on the panel at the brightest scene frame; assert ≥7:1; capture frame time before/after.
- **Effort** S · **Risk** low.

## BUG-027 · Scanlines are painted over the UI
- **Severity** Medium · **Type** rendering/stacking
- **Evidence** `ui/surface.css:15-16` (`.scan{z-index:20; mix-blend-mode:multiply; 1px/3px}`) above chat (z9) and terminal (z8); `.22` alpha while booting.
- **Root cause** A full-window CRT layer was stacked above the panels instead of below them.
- **Fix** Move `.scan` to `z-index:1` (above the canvas, below panels) or scope it to `#stage-region` as a pseudo-element; keep the boot variant only over the boot overlay.
- **End state** UI text is crisp; the CRT texture remains on the artwork only.
- **Why** Darkening every third pixel row of text is a permanent legibility tax.
- **Depends** BUG-020 · **Verify** V-PNG: text row luminance variance drops; `.scan` has no intersection with panel rects.
- **Effort** XS · **Risk** very low.

## BUG-028 · The terminal is a fixed card with no dock or resize
- **Severity** High · **Type** layout/interaction
- **Evidence** `ui/station.css:78-79` (fixed size, `overflow:hidden`); `ui/station.js:209-224` only toggles `hidden`; `:225` is the only geometry hook; the host already supports cols/rows resize.
- **Root cause** The backend is resizable; the front end refuses to express it.
- **Fix** L0 dock with a draggable splitter (pointer events, 120 px minimum per side), double-click to maximise the terminal, `data-dock` persisted in `window_state`. Fit xterm on splitter move (debounced) and on dock change.
- **End state** A terminal that behaves like a docked panel: resize, maximise, remember.
- **Why** Coding work is split between chat and terminal; a fixed card forces one to cover the other.
- **Depends** BUG-024 · **Verify** V-FLOW: drag the splitter, assert cols/rows change and the pty receives the resize; restart and assert the ratio is restored.
- **Effort** M · **Risk** low.

## BUG-029 · The boot intro never plays
- **Severity** Medium · **Type** motion/first run
- **Evidence** `ui/station.js:390` calls `boot(done, true)`; `ui/scene.js:245-246` short-circuits; `:247-257` is dead code; CSS title waits 0.9 s + 1.1 s while `#boot` fades out at 0 s (`ui/surface.css:19-23`).
- **Root cause** The page passes `instant=true` and flips `booting→booted` before first paint; the CSS and JS timelines were never reconciled.
- **Fix** Drive boot from the host: show the title for 900 ms, start the scene fly-in, then fade the overlay when the scene reports `ready` (or after a 2.5 s cap). Remove `instant` from the production call; keep it for tests. Sequence CSS fades off the same `data-phase` attribute.
- **End state** A deliberate 2–3 s entrance that matches the wordmark, with no cut-off.
- **Why** First impressions set the perceived quality of everything after.
- **Depends** BUG-030 (motion authority) · **Verify** V-PNG captures at 0.4/1.2/2.4 s show title→scene→chat, and `body.booted` is set only after the fade.
- **Effort** S · **Risk** low; tests must opt into `instant`.

## BUG-030 · Reduced motion is only half honoured
- **Severity** Medium · **Type** a11y/motion
- **Evidence** `ui/station.js:5` samples the media query once; `:389` applies it once; `ui/surface.css:30-31` pauses animations but `animation:none` removes the fill that makes the boot title visible; the RAF loop still reapplies camera parallax on each wake; `setEnergy` always wakes.
- **Root cause** Motion is a one-time flag, not a live preference, and several pathways bypass it.
- **Fix** Listen to `matchMedia(...).change` and re-apply; make reduced motion mean "static, not invisible" (keep final fills); gate parallax and glitch; make `setEnergy` update state without waking the loop when motion is off; expose a Settings toggle that overrides the OS.
- **End state** With reduced motion, the app is fully visible and still; toggling the OS preference updates it live.
- **Why** Accessibility and comfort; the current behaviour produces an invisible title.
- **Depends** none · **Verify** Toggle the emulated media feature in the GUI harness; assert camera position is constant and the title is visible.
- **Effort** S · **Risk** low.

## BUG-031 · One message can trigger two glitches
- **Severity** Medium · **Type** state/motion feedback
- **Evidence** `ui/scene.js:265-269` glitches on any upward jump > .5; `ui/station.js:154` sets energy optimistically; `:130` sets it again on the polled `thinking`; `:135-139` reset to `.1`; `src/chief.rs:767` emits `thinking` per clause.
- **Root cause** Two independent energy drivers plus per-clause events.
- **Fix** Single driver: move energy to a host-computed `phase` (`idle|working|error`) in the status payload; remove optimistic `setEnergy`; glitch only on `idle→working` once per turn; emit `thinking` once per turn.
- **End state** One transition, one glitch; multi-clause commands do not strobe.
- **Why** Gratuitous tearing reads as instability.
- **Depends** BUG-001 (phase events) · **Verify** Submit a two-clause command; assert exactly one `glitch` call (instrument a counter in the harness).
- **Effort** S · **Risk** low.

## BUG-032 · Pointer parallax tracks the whole viewport
- **Severity** Low–Medium · **Type** interaction/motion
- **Evidence** `ui/scene.js:167` listens on `window` and normalises by `innerWidth/Height`; `:234-236` lerps it every frame; `:198` applies ±0.45 world units; the canvas itself is `pointer-events:none`.
- **Root cause** A global listener with no stage-rect awareness and a fixed amplitude.
- **Fix** Ignore pointer events outside the stage rect and while the composer has focus; scale amplitude by stage size; add a dead zone and a slower ease; disable entirely under reduced motion.
- **End state** Typing in the chat does not move the artwork; moving over the stage gives a subtle parallax.
- **Why** A camera that reacts to unrelated input feels unstable.
- **Depends** BUG-020, BUG-030 · **Verify** Instrument `camera.position`; move the pointer over the composer and assert no change.
- **Effort** S · **Risk** low.

## BUG-033 · `hidden` does nothing on the terminal pane and session strip
- **Severity** High · **Type** CSS cascade
- **Evidence** `ui/index.html:13-14` set `hidden`; `ui/station.css:71,79` set `display:flex`; no `[hidden]` rule exists in either stylesheet; `ui/station.js:386` inspects the *attribute*, so QA reports "closed" while the empty card is painted. Visible in `evidence/isolated-gui.png` with no session opened.
- **Root cause** Author `display:flex` outranks the UA `[hidden]{display:none}` by origin, so the attribute only changes the DOM property.
- **Fix** Add `[hidden]{display:none!important}` to `surface.css`; make dock state authoritative via `body[data-dock="closed"]`; change `inspect()` to report computed `display`.
- **End state** At boot the stage is unobstructed; Close actually closes; QA tells the truth.
- **Why** An empty blurred card permanently covering the artwork, plus a close button that does nothing, is both a visual and a trust defect.
- **Depends** none (land first) · **Verify** V-PNG with no session: pixels at the card's former centre are stage, not glass; `inspect().terminal === false`; open/close asserts `display:none`.
- **Effort** XS · **Risk** very low.

---

### Order of work
1. BUG-033 (one line; changes every before/after capture).
2. BUG-030 (motion authority), then BUG-029 (boot timeline).
3. BUG-020 (L0 + frame contract + dim ownership), then BUG-021/BUG-024/BUG-025.
4. BUG-028 (dock interaction), BUG-022/BUG-023 (stage presentation).
5. BUG-027/BUG-026 (stacking, surfaces), BUG-031/BUG-032 (motion feedback).

All scene-side plans require `npm run bundle` before any capture.
