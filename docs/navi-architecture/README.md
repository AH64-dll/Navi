# NAVI

Architecture and executable integration checks for a browser-first assistant built around [Pi](https://github.com/earendil-works/pi).

**There is no runnable NAVI browser extension or native host yet.** The current work makes the design concrete and tests selected contracts against Pi's published packages.

- **[NAVI-ARCHITECTURE.md](NAVI-ARCHITECTURE.md)** is the authoritative implementation plan: product scope, browser permissions, native installation/protocol, lifecycle, Pi configuration, limits, Sodium/WebMCP and acceptance gates.
- **[Implementation review](research/11-implementation-review.md)** explains the corrected assumptions and records current external evidence.
- **[Validation](validation/README.md)** contains executable native-framing, document-binding, budget and offline Pi SDK checks.
- **[Original research](research/)** and the **[archived architecture](research/archive-2026-09-11-architecture.md)** preserve the earlier investigation. They are historical, may contradict each other, and do not override the current plan.

Start with deliberate page/selection questions, bounded follow-ups and passage references. One browser-spawned native host embeds one Pi session with only NAVI's page tool. Memory, proactive observation and browser actions require separate evidence of usefulness.

[Sodium](https://sodium.result.dev/) is a promising producer of website WebMCP tools. NAVI would consume those tools through an optional browser adapter; the Sodium website SDK is not a universal extraction engine. The page assistant must work when WebMCP is unavailable.

Run checks with Node 22.20.0 or a compatible newer runtime:

```powershell
npm ci --ignore-scripts --no-audit --no-fund
npm test
```

Installation downloads exact locked development dependencies. Tests use Pi's scripted faux provider, without model credentials. To check transport and policy invariants without dependencies, run `npm run test:contracts`.

Source research uses Pi commit `71dca871bc80b6bc97be37f0ca3189399d651fff`; executable checks use `@earendil-works/pi-coding-agent@0.85.1` and `@earendil-works/pi-ai@0.85.1`. A source checkout under `vendor/` is optional and is not needed to run the checks.
