# Cross-review — NAVI v1 memory (adjudication of 05 vs 09 vs 10)

All repo claims re-verified in `C:\Users\lain\NAVI\vendor\pi` @ `71dca871`; read-only, nothing modified.

## Verdict

Agents 5 and 10 agree on the load-bearing point: **memory must not live in Pi session storage**. Agent 9 is right about v1 *scope*, wrong about v1 *placement*.

**Adopted: Agent 10's placement rule, enforced by Agent 5's store shell, populated to Agent 9's scope.**

- **Store:** one NAVI-owned SQLite DB (`~/.navi/memory/store.sqlite`; WAL, `BEGIN IMMEDIATE`, one designated writer process). Not Pi sessions; not JSONL as the serving store.
- **Retrieval:** explicit pins + structured filters (time, domain, tab/session) + SQLite FTS5 when the build reports it, with a `page_terms` fallback. **No embeddings, no RRF, no vector index in v1.**
- **Capture:** explicit-first. Ambient observation stores **metadata only** (`url_key`, domain, title, timestamps, dwell, tab). Page *content* is stored only on an explicit gesture ("remember this", highlight, clip), as a bounded extract + note — removing most of the privacy surface and storage bill, and leaving FTS a small, high-precision corpus.
- **Where it runs:** content script extracts → extension service worker coalesces and buffers → **NAVI bridge writes**. The Pi extension is a **reader only**.
- **APIs:** model tools `navi_remember` and `navi_recall`; `navi_forget` stays **user-only** in v1 (extension command/UI).

"Remember this" is a write to `pins`, not a session entry. "The page I looked at earlier" is a structured + lexical query where *the user's own words carry the semantics* — no embeddings.

## Evidence

- **Undeletable, cwd-keyed storage.** Entries "cannot be modified or deleted" (`core/session-manager.ts:1310-1317`); the only sanctioned exception is the *precise rewrite*, of which "no implementation exists" (`agent/docs/harness.md:578-582`). The partition key is cwd (`:476-481`), and a stored cwd that no longer exists is a hard error (`core/session-cwd.ts:14-33`). Pages are not directories; memory keyed this way fragments per shell.
- **Synchronous, structural writes.** `_appendEntry` (`:1058-1063`) pushes, advances the leaf, then `_persist` (`:1029-1056`) → `appendFileSync` (`:1035`, `:1054`). Each ambient observation would block the event loop and become tree history that `/tree` navigates and `/fork` copies.
- **The extension surface is read-only plus `appendEntry(customType, data)`** (`core/extensions/types.ts:319`, `:1381`). `custom` is durable and out of context (`session-manager.ts:104-108`); `custom_message` is durable and **in** context (`:135-141`) — the two-tier split NAVI needs (one `navi.recall`, one `navi.context`).
- **No always-on component can be an extension:** the factory must not start processes, sockets, watchers or timers; defer to `session_start`, close in `session_shutdown` (`docs/extensions.md:220-224`). An extension also lives only while a Pi invocation does.
- **Search is a skeleton whose design matches my adoption:** standalone service, own store, pull-based catch-up on a durable cursor, "events are only hints", "a rebuildable projection with zero authority" (`agent/src/search/index.ts:20-27`; `agent/docs/harness.md:568-576`); post-filtering after a ranked `limit` is **unsound** (`post-wp05-roadmap.md:102-110`).
- **SQLite backend scope:** `entries(seq, type, custom_type, payload)` plus integrity triggers, **no FTS5 table, no embedding column**; "does not export a search service or FTS index" (`sqlite-node/README.md:33`; `migrations/001_initial.sql`). Repo-wide `fts5|embedding|vector` grep: no implementation. One writable owner is a deployment invariant, not a backend feature (`:31`).

## Why the losing option remains attractive

Agent 5's stack is not wrong, only premature. Vectors + RRF + decay buy **thematic** recall ("what do I know about X"), the product's real long-term premise, at a small honest cost (i8 vectors; brute-force cosine is milliseconds at this scale). Lexical recall only answers questions where the user *names the thing*; ask by idea instead and FTS returns nothing. The counter: Agent 9's "branch + file memory" is also a store, just a worse one — it fails both target phrases once history outgrows a directory. **The loser is the vector tier, not the store.**

## Conditions/triggers to switch

Add embeddings + RRF (brute-force, local `all-MiniLM-L6-v2`, candidate-restricted) when **any two** hold:
1. Content corpus (pins + extracts + digests) exceeds ~5k rows with >1k logged queries.
2. Measured paraphrase-miss: on a hand-labelled 100-query set, precision@3 < 0.6 while the correct page *is* in the store (vocabulary mismatch, not absence).
3. Digests become default-on — vectors then pay for themselves and the privacy call is already made.

Also switch if SW wake latency exceeds ~500 ms p95 (writer moves to native messaging), or if NAVI must index Pi transcripts too (adopt S3 verbatim, generation-keyed cursors).

## Top 3 failure modes

1. **Wrong store.** Ambient events as Pi entries → undeletable, sync-blocking, cwd-fragmented, uploaded by `/share`. *Mitigation:* separate DB; Pi gets ≤2 entries per conversation; an export filter drops `navi.*`; **no `context` handler registered** (fires per LLM call and forces a full-conversation `structuredClone`, `runner.ts:1034-1036`).
2. **Confident false recall.** A stale page or merged `url_key` asserted as fact. *Mitigation:* every hit carries url + title + `when` + "recalled, unverified"; injection is tail-appended only (`harness.md:518`); only pinned items surface unprompted.
3. **Silent capture loss under MV3 suspension.** Chrome suspends the service worker in ~30 s; naive designs record only focused tabs. *Mitigation:* the bridge owns the writer as a real process; the SW buffers to `chrome.storage` with a monotonic seq and replays on wake; a pull-based sweep of `chrome.history` on a durable cursor makes lost pokes harmless — exactly S3's "events are hints" rule.

## Confidence

**High (~0.8)** on placement (memory must not live in Pi sessions) and on v1 sufficiency (pins + structured filters + FTS answer both phrases). **Medium (~0.6)** on deferring vectors — the paraphrase-miss threshold needs measurement, not argument. **Medium (~0.55)** on MV3 wake/buffering, not verifiable from this repo. **Low (~0.4)** on TTL numbers.
