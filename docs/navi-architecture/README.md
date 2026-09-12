# NAVI — Architecture Research & Plan

Design research for **NAVI**, an ambient, browser-first AI harness built around [Pi](https://github.com/earendil-works/pi) — inspired by NAVI from *Serial Experiments Lain*.

**Planning only — no NAVI implementation exists yet.** This directory contains research and architecture, not code.

## Contents

- **[NAVI-ARCHITECTURE.md](NAVI-ARCHITECTURE.md)** — the main deliverable: recommended architecture, three candidate architectures, the feature decision matrix, minimal primitives, staged development roadmap, first prototype, and performance / security / cross-platform strategy, plus rejected ideas and open questions.
- **[research/](research/)** — the supporting evidence:
  - `01-core-architecture.md` … `10-red-team.md` — ten independent research reports covering Pi's core runtime, integration modes, browser perception, context intelligence, memory, security, performance, UX, capabilities, and a red-team review.
  - `xreview-1-substrate.md` … `xreview-7-agents.md` — independent adjudications of the seven contested decisions (substrate, activation lifecycle, bridge transport, page representation, memory scope, first surface, agent topology).

## Method

- **Evidence base:** read-only clone of `github.com/earendil-works/pi` @ `71dca871` (`main`, 2026-09-11), `@earendil-works/pi-coding-agent v0.85.1`. Every report cites `file path → symbol → implication` and tags claims `[verified]`, `[inferred]`, or `[external]`.
- **Process:** ten parallel research agents with separated investigation areas, then seven independent cross-reviews on the disagreements. Consensus was not forced; unresolved uncertainty is recorded with confidence levels.

## Recommended direction (short version)

- A **browser extension** owns perception, page actions and consent; a lazily-spawned **local NAVI core** owns lifecycle, budgets, memory, policy and presentation; **Pi, unmodified and pinned, is embedded via the SDK** as the reasoning engine, with stdio RPC as an isolated second lane.
- **No fork, no core changes.** Everything maps onto Pi's two declared-stable surfaces (SDK + stdio RPC); a short list of optional upstream fixes is included.
- **OBSERVE-first** capability model (Scope × Verbs × Mode × Taint); page content is untrusted data; memory is a NAVI-owned store, never Pi session storage.
- **Seven primitives** instead of thirty features; budget- and cache-aware by design; the first prototype is small enough to lose against a "dumb extension" baseline, on purpose.
