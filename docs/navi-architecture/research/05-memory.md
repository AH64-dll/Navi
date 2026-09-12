# NAVI Agent 5 of 10 — Memory and User Context

**Scope:** design research only, no implementation.
**Repo under study:** `C:\Users\lain\NAVI\vendor\pi` — read-only clone of `github.com/earendil-works/pi`, branch `main`, commit `71dca871` (2026-09-11); `@earendil-works/pi-coding-agent` v0.85.1 (`packages/coding-agent/package.json:3`). Not modified.
**Paths note:** the task referred to `src/core/session-manager.ts` and `docs/sessions.md`; in this tree those live at `packages/coding-agent/src/core/session-manager.ts` and `packages/coding-agent/docs/*.md`. All citations below use the real paths (the file is 1746 lines, not 1561 — the task's line count predates this commit).

Evidence tags: **[verified]** = read in source; **[inferred]** = my reasoning over verified source; **[external]** = outside the repo, sourced at the end.

---

## Repository Findings

### 1. What Pi persists, and in what shape

- `packages/coding-agent/src/core/session-manager.ts:856` → `class SessionManager` (docblock 845-855) → sessions are **append-only trees in JSONL**: every entry has `id`/`parentId`, the "leaf" pointer marks the current position, appending creates a child of the current leaf, and branching only moves the leaf. History is never rewritten. **[verified]**
- `session-manager.ts:30` → `CURRENT_SESSION_VERSION = 3` → three format generations, migrated on load (`migrateV1ToV2` 230-257, `migrateV2ToV3` 259-275, driver `migrateToCurrentVersion` 281-291, invoked from `_loadEntries` 961-963) → Pi accepts format evolution as a first-class, load-time concern. **[verified]**
- `session-manager.ts:144-153` → `type SessionEntry` union → exactly **nine** entry types: `message`, `thinking_level_change`, `model_change`, `compaction`, `branch_summary`, `custom`, `custom_message`, `label`, `session_info`. The type set is closed; extensibility is via `customType` string on two of them. **[verified]**
- `session-manager.ts:104-108` → `interface CustomEntry` (`type:"custom"`, `customType`, `data`) → "Does **NOT** participate in LLM context (ignored by buildSessionContext). For injecting content into context, see CustomMessageEntry." **[verified]**
- `session-manager.ts:135-141` → `interface CustomMessageEntry` → `type:"custom_message"` **does** enter LLM context; `sessionEntryToContextMessages` (383-408) converts it into a `custom`-role message, `display` merely controls TUI rendering. **[verified]** → Pi already has the exact two-tier split NAVI needs: durable-but-invisible state vs durable-and-visible context.
- `session-manager.ts:110-115` + `1246-1267` (`appendLabelChange`) + `981-989` (`_buildIndex` label resolution) → `LabelEntry` targets another entry by id, latest-wins, and is a **real tree entry** that can have children (hence the re-chaining logic in `createBranchedSession` 1434-1460). **[verified]**
- `session-manager.ts:1029-1056` → `_persist(entry)` → writes are **deferred until the file already contains an assistant message**; before that, entries are held in memory (`flushed=false`) and flushed in one batch (`openSync(path,"wx")` at 1044). Appends afterwards use `appendFileSync` (1035, 1054). **[verified]** → a session that never gets a model response never hits disk, and early extension entries are batched, not streamed.
- `session-manager.ts:1044` → `openSync(this.sessionFile, "wx")` → exclusive-create for the first full write. There is **no lock file, no fsync, no atomic rename, no reader/writer coordination** in `SessionManager`. **[verified]**
- `packages/coding-agent/src/experimental/session-worker.ts:533` → `await lockfile.lock(metadata.path, { realpath:true, stale:2_000, update:1_000, retries:{retries:320,...} })` → the **only** per-session exclusive lock in the repo, and it lives in the experimental worker host, not in the session layer. **[verified]** → "locking" for sessions is a host-lifecycle concern.
- `packages/session-backends/sqlite-node/README.md:31` → "The host lifecycle, not this backend, guarantees one writable owner per Session … implements no cross-process lease, lock, fence, heartbeat, or takeover." **[verified]** → consistent: single-writer is an invariant the *deployment* must hold.
- `session-manager.ts:476-489` → `getDefaultSessionDirPath` → `~/.pi/agent/sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`; `packages/coding-agent/src/config.ts:572` → `getSessionsDir()` = `join(getAgentDir(), "sessions")`, `getAgentDir()` = `~/.pi/agent` (config.ts:528-534). Documented at `docs/session-format.md:7-11`. **[verified]** → **cwd is the partitioning key** for history.
- `packages/coding-agent/src/core/session-cwd.ts:14-33` → `getMissingSessionCwdIssue` → a stored cwd that no longer exists is a first-class error (`MissingSessionCwdError`, 44-58) → Pi treats the cwd as durable identity, not decoration. **[verified]**
- `session-manager.ts:636-657` → `findMostRecentSession(sessionDir, cwd)` → "resume the last session" = `readdirSync` + bounded header read (`MAX_SESSION_HEADER_SCAN_BYTES = 1 MiB`, line 494) + mtime sort; `SessionManager.list` (1670-1679) and `listAll` (1687-1745) fully parse every file. **[verified]** → **discovery is a linear file scan; there is no catalog.** Cost scales with number of sessions, not with bytes indexed.
- `session-manager.ts:688-766` → `buildSessionInfo` → the only derived search artifact in coding-agent: `allMessagesText` = user+assistant text concatenated (733, 761), plus `name`, `cwd`, `firstMessage`, `messageCount`, `modified` (max message timestamp, 721-724). All computed **in RAM per listing and thrown away**. **[verified]**
- `session-manager.ts:461-470` (`buildSessionContext`) and `418-454` (`buildContextEntries`) → context = walk leaf→root; the **latest compaction replaces everything before `firstKeptEntryId`**. **[verified]**
- `session-manager.ts:1315-1317` → `getEntries()` docblock: "The session is append-only: use appendXXX() to add entries, branch() to change the leaf pointer. **Entries cannot be modified or deleted.**" **[verified]**
- `packages/agent/docs/harness.md:578-582` → the "precise rewrite" (copy the retained set into a fresh store and swap) is "**the sole sanctioned exception**" to never-delete, and "**no implementation exists**". **[verified]** → erasure/redaction in the Pi data model is *tooling above the runtime*, not a runtime mechanism.
- `session-manager.ts:164-166`, `1324-1362` → `SessionTreeNode` / `getTree()` → labels are resolved onto nodes and carried with `labelTimestamp`; children sorted by timestamp; iterative traversal to survive deep trees. **[verified]**

### 2. What the agent (extension) layer can actually do

- `packages/coding-agent/src/core/extensions/types.ts:319` → `sessionManager: ReadonlySessionManager` → the extension-facing surface is read-only. The exact list (`session-manager.ts:190-206`): `getCwd`, `getSessionDir`, `getSessionId`, `getSessionFile`, `getLeafId`, `getLeafEntry`, `getEntry`, `getLabel`, `getBranch`, `buildContextEntries`, `getHeader`, `getEntries`, `getTree`, `getSessionName`. **No search, no write, no listing.** **[verified]**
- `extensions/types.ts:1381` → `appendEntry<T = unknown>(customType: string, data?: T): void` → the sanctioned persistence primitive; `loader.ts:375-378` forwards it to `runtime.appendEntry` → `SessionManager.appendCustomEntry` (`session-manager.ts:1136-1147`). **[verified]**
- `extensions/types.ts:1358` → `registerEntryRenderer<T>(customType: string, renderer: EntryRenderer<T>): void`; `loader.ts:351-355` stores it in a per-extension `Map<string, EntryRenderer>` → custom entries can be *rendered* but still do not reach the LLM. `docs/extensions.md:1624-1642` shows the intended pairing. **[verified]**
- `docs/extensions.md:1477-1493` → the restore idiom: on `session_start`, scan `ctx.sessionManager.getEntries()` for `entry.type === "custom" && entry.customType === "<mine>"` and rebuild in-memory state from `entry.data`. **[verified]** → Pi's answer to "remember this across restarts" is *replay a projection from the log*, not a side store.
- `docs/extensions.md:1424` → `pi.sendMessage()` injects a `custom_message` (in context); `pi.appendEntry()` is "durable TUI-only content that should not be sent to the LLM". **[verified]**
- `docs/extensions.md:156-178` and `docs/extensions.md:222-224` → extensions are loaded by jiti; the factory runs in invocations that never start a session; "**Do not start background resources such as processes, sockets, file watchers, or timers from the factory**" — defer to `session_start` (or a command) and register an idempotent `session_shutdown` handler. **[verified]** → a NAVI indexer/daemon must **not** boot from the extension factory.
- `docs/extensions.md:675-685` → the `context` event: "Fired before each LLM call. Modify messages non-destructively", `event.messages` is a deep copy. **[verified]** → this is the correct injection point for retrieved memory.
- `docs/extensions.md:530-565` → `before_agent_start` can inject a persistent `custom_message` and rewrite the system prompt per turn. **[verified]**
- `docs/extensions.md:1385-1420` → `pi.registerTool({name,label,description,parameters,execute,...})`, `execute` returns `{content, details}` and may stream via `onUpdate` → the mechanism for exposing a retrieval API to the model. **[verified]**
- Event vocabulary available for ingest (verified in `docs/extensions.md:277-349`, 393-735): `project_trust`, `session_start`, `session_info_changed`, `session_before_switch`, `session_before_fork`, `session_before_compact`/`session_compact`/`session_compact_failed`, `session_before_tree`/`session_tree`, `session_shutdown`, `before_agent_start`, `agent_start`/`agent_end`/`agent_settled`, `turn_start`/`turn_end`, `message_start`/`message_update`/`message_end`, `tool_execution_*`, `tool_call` (can block), `tool_result` (can modify), `context`, `before_provider_headers`, `before_provider_request`, `after_provider_response`, `model_select`, `thinking_level_select`, `input`, `user_bash`, `ui_prompt_start`/`ui_prompt_end`. **[verified]**

### 3. Session export and share

- `packages/coding-agent/src/core/session-export.ts:7-42` → `exportSessionToJsonl(sessionManager, outputPath?, createTrailingEntries?)` → writes a fresh header, then **linearizes the branch** (`...entry, parentId: <prevId>`), then appends exporter-supplied trailing entries. **[verified]**
- `packages/coding-agent/src/modes/interactive/session-share.ts:25-43` → `exportSessionForShare` appends a trailing `{type:"custom", customType:"pi.share", data:{systemPrompt, tools}}` entry; `107-120` → uploads the ndjson to Radius `/v1/artifacts` with `visibility=organization`; `148-192` → otherwise `gh gist create --public=false`. **[verified]** → **everything in the session, including any `custom` entry an extension appended, is uploaded by `/share`.** This is the single most concrete privacy hazard for NAVI memory written as session entries.
- `packages/coding-agent/docs/sessions.md:50` and `docs/session-format.md:13-17` → deletion is manual: `/resume` + `Ctrl+D` (using the `trash` CLI) or deleting `.jsonl` files. **[verified]** → **Pi has no retention policy, no TTL, no automatic pruning, and no dead-byte reclamation.**

### 4. The SQLite backend: real, but not a search engine

- `packages/session-backends/sqlite-node/src/sqlite/migrations/001_initial.sql:9-121` → real schema: `sessions`, `entries(session_id,id,parent_id,seq,type,custom_type,timestamp,payload)` + `ix_entry_parent`/`ix_entry_seq`, `scalar_values`, `list_values`, `usage_ledger`, and projections `branch_entries`/`branch_meta`. **[verified]**
- Same file, 66-91 → integrity is enforced by **triggers** (`trg_entries_validate` rejects a missing parent and cross-table id collisions). **[verified]** → the backend is production-shaped, not a sketch.
- `packages/session-backends/sqlite-node/src/sqlite/repo.ts` → `SqliteSessionRepo` (`create`/`open`/`list`/`delete`/`fork`) with `PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;` for writable connections and `busy_timeout` only for read-only. One `.sqlite` file per session by default; a shared container via `databasePath`. **[verified]**
- `packages/session-backends/sqlite-node/README.md:33` → "The package **does not export a search service or FTS index**; search is the separate S3 projection." **[verified]**
- **There is no FTS5 table, no embedding column, and no vector index anywhere in the repo.** The only `FTS5`/`MATCH` mentions are in `packages/agent/docs/harness.md`, `packages/agent/docs/post-wp05-roadmap.md`, and minified vendor JS (`packages/coding-agent/src/core/export-html/vendor/highlight.min.js`). **[verified]**

### 5. Search: a design, a skeleton, and nothing else

- `packages/agent/src/search/index.ts:1-27` → the *entire* implementation is types: `SearchQuery{text,limit?}`, `SessionSearchHit`, `EntrySearchHit`, `SessionSearchService{searchSessions, searchEntries?, sync, notify, remove, close}`. **[verified]**
- `packages/agent/docs/post-wp05-roadmap.md:102-110` → S3 evidence: "There is **no factory, sync utility, cursor store, projection, or source SQLite FTS implementation**." The draft interface "conflicts with the newer search design and has no implementation". Constraints stated: "**Post-filtering after ranked `limit` is unsound**" and "implement standalone catch-up and a separate SQLite FTS5 projection; **do not add repository search methods**." **[verified]**
- `packages/agent/docs/harness.md:568-576` → the normative S3 design, which is a near-perfect template for NAVI memory:
  - search is a **standalone service with its own store**; the repository "knows nothing about it and exposes no search methods";
  - **pull-based indexing; events are only hints** — a notify carries no content, a lost poke is caught by the next sweep;
  - a **durable per-session cursor** = highest indexed `entry seq`; sync reads `scanEntries({fromSeq: cursor+1})`, indexes idempotently per `(sessionId, entryId)`, and advances the cursor **in the same store transaction** → crash mid-batch re-indexes into the same state;
  - cursors key on `(sessionId, storeGeneration)` because the precise rewrite can renumber seqs; a generation mismatch triggers full re-index;
  - reference implementation = "one standalone SQLite database — an FTS5 table over `(session_id, entry_id, text)` plus the cursor table — working unchanged over JSONL session files; several processes may share it (WAL, `busy_timeout`, `BEGIN IMMEDIATE`, idempotent rows, monotonic cursor updates; writers serialize)";
  - "The index is a **rebuildable projection with zero authority**; indexing failures never affect the harness or commits."
  - Open question left open: **metadata filtering** (`cwd`) — four candidates listed, (c) "post-filter in the app" marked **unsound**. **[verified]**
- `packages/agent/docs/harness.md:518` → "**Append-only context invariant.** Across one lane's requests, provider context must only grow at the tail: an insertion before the previous request's tail invalidates the provider's KV cache and multiplies cost." **[verified]** → injected memory must be **tail-appended**, never spliced into the middle of context.
- `packages/agent/docs/harness.md:1251` → Part 6 "Future: partitioned retention (Postgres)"; `post-wp05-roadmap.md:221` lists "a partitioned Postgres backend/retention policy" as deferred. **[verified]** → Pi has *decided not* to solve long-horizon retention yet; NAVI cannot borrow one.

### 6. Retrieval that actually works today

- `packages/coding-agent/src/modes/interactive/components/session-selector-search.ts:26-28` → `getSessionSearchText(session)` = `` `${session.id} ${session.name ?? ""} ${session.allMessagesText} ${session.cwd}` `` → retrieval operates on a **flattened, in-RAM string per session**. **[verified]**
- Same file, 39-114 (`parseSearchQuery`) and 116-154 (`matchSession`) → the whole matcher: `re:<pattern>` regex mode; quoted `"phrase"` mode with whitespace normalization; otherwise `fuzzyMatch` from `@earendil-works/pi-tui` (line 1); score = match character offset × 0.1, lower is better. **[verified]**
- Same file, 156-194 (`filterAndSortSessions`) → three sort modes `threaded | recent | relevance`; relevance sorts by score then `modified` desc; name filter `all | named`. **[verified]**
- `packages/coding-agent/test/session-selector-search.test.ts:22-193` → what is **proven**: phrase matching across newlines, regex matching, recent-order preservation, relevance ordering + tie-break, invalid regex → empty, and the named filter. **[verified]** → nothing at persistence level, nothing semantic, nothing ranked by meaning.
- `packages/coding-agent/test/session-manager/load-entries.test.ts` (adopt entries verbatim, keep leaf, avoid id collisions, rebuild branch structure, **rebuild labels**, resolve a compaction against `firstKeptEntryId`) and `save-entry.test.ts` (custom entry lands in the tree but `buildSessionContext()` returns only message entries) and `labels.test.ts` (last label wins, labels survive `createBranchedSession`, labels excluded from context) → **the proven retrieval primitives are: walk the tree, resolve labels, resolve the compaction cut, filter entries by type.** **[verified]**
- `packages/coding-agent/src/modes/interactive/components/session-selector.ts:206-231` → `buildSessionTree(sessions)` builds a **session lineage tree from `parentSessionPath`**; `:293` → default `sortMode = "threaded"`; `:134` labels the modes. **[verified]** → Pi already ships a two-level *relationship graph over sessions* (fork/clone lineage), rendered as a tree in `/resume`.
- `packages/coding-agent/src/experimental/services/sessions.ts:4-33` → `SessionAddress{serverId,sessionId}`, `SessionSummary extends SessionAddress {createdAt}`, `SessionDirectoryState{revision, sessions}`, and `defineService<SessionManagement>("pi.session-management")` with `create/remove/attach/detach` → a **revisioned, replicated directory of live sessions** already exists at the server layer. **[verified]**
- `packages/coding-agent/src/experimental/services/sessions.ts` exposes no content, no relationships, and no query — only identity + lifecycle. **[verified]**

### 7. The three extension examples the task named

- `packages/coding-agent/examples/extensions/bookmark.ts:19-27` → `/bookmark` scans entries backwards for the last assistant message and calls `pi.setLabel(entry.id, label)`; `/unbookmark` (34-48) clears the first label found walking backwards. **[verified]** → "bookmark" in Pi is *an entry label*, navigable only inside `/tree`, with no content and no retrieval by meaning.
- `examples/extensions/session-name.ts:19-23` → `/session-name` wraps `pi.setSessionName` / `pi.getSessionName` → session-level titling is a single latest-wins string (`SessionInfoEntry`, `session-manager.ts:118-121`, read back by `getSessionName` 1164-1175), and it is what `/resume` displays instead of the first message (`docs/session-format.md:315`). **[verified]**
- `examples/extensions/summarize.ts:68-115` → `buildConversationText(entries)` flattens `getBranch()` into `User:`/`Assistant:`/`Tool <name> was called with args <json>` lines; `:149` reads `ctx.sessionManager.getBranch()`; `:163-189` does a **one-shot** `ctx.modelRegistry.complete(...)` with `cacheRetention:"none"` and a fresh `sessionId: uuidv7()`; `:117-143` renders the result in a modal. **[verified]** → the one shipped pattern for compressing history: read the branch, summarize out-of-band with cache writes disabled, and **throw the result away** (it is never persisted).

### 8. Storage-growth precedent inside Pi

- `packages/agent/docs/harness.md:57` and `post-wp05-roadmap.md:53-65` → J1: "temp-file-and-rename snapshot compaction" is specified but **not implemented**: "Superseded `pi.op.state`, deleted pending payloads, deleted tool checkpoints, and deleted assistant-frame lists remain physical bytes **indefinitely**." **[verified]**
- `post-wp05-roadmap.md:160-170` → the authors' own amplification methodology, with concrete measured numbers from a 303,920-byte / 569-line session: 477 assistant-frame appends ≈ 118,418 serialized write bytes; 148,214 physical bytes mentioning a deleted frame namespace; ≈51,568 bytes of superseded writes. They require the fix to "add a deterministic repository fixture and measurement script that reproduces or replaces these figures". **[verified]** → the repo's own standard for memory-like features is a checked-in amplification measurement, not a prose claim.
- `packages/agent/docs/harness.md:489` + `packages/agent/docs/values.md:5-20` → application durable state = **bound typed addresses** `value<T>(namespace, key?)` / `list<T>(namespace, key?)`, and: "Applications define their own stable collision-resistant addresses … there is **no built-in application namespace or separate application-state API**." Fork/migration/cleanup policy belong to the consumer (`values.md:66`). **[verified]** → this is the sanctioned place for a small per-session NAVI slice if NAVI is built on the Pi/AgentHarness storage model.

---

## Architectural Implications

**I1 — Browsing memory must not live in Pi session files.** Four independently verified properties make session JSONL the wrong home for an ambient log:
1. entries are immutable and undeletable (`session-manager.ts:1315-1317`), and the only sanctioned deletion path is the **unimplemented** precise rewrite (`harness.md:578-582`);
2. the whole file is read into RAM on open (`loadEntriesFromFile` 514-557) and fully parsed again for every listing (`buildSessionInfo` 688-766);
3. every append moves the leaf and creates a tree node (`_appendEntry` 1058-1063), so memory appends would become structural history that `/tree` navigates and `/fork` copies;
4. everything in the file is exported and uploaded by `/share` (`session-share.ts:26-43`, `107-120`).
Consequence: a per-page memory written as `custom` entries would grow without reclamation, slow every resume, pollute tree navigation, and leak into shared links. **[inferred, from @verified]**

**I2 — The right Pi-shaped home for memory is the S3 pattern, not the session store.** S3 (design-only, zero code) already specifies exactly the shape an ambient memory index needs: separately owned store, pull-based catch-up with a durable cursor, idempotent upserts keyed by `(source, id)`, generation-aware invalidation, events as hints only, failures never affecting the host, and "the index is a rebuildable projection with zero authority" (`harness.md:570-574`). NAVI memory *is* that shape with a different source (browser events instead of session entries). **[inferred]**

**I3 — Memory has to be a two-tier system, and Pi already proves the split.** `custom` (durable, out of context) vs `custom_message` (durable, in context) (`session-manager.ts:104-141`) maps onto **observed** (cheap, ambient, never auto-injected) vs **promoted/recalled** (explicitly surfaced, injected with provenance). Promotion is a deliberate act with a token cost; observation is free-ish and bounded. **[inferred]**

**I4 — Injection must be tail-appended and budgeted.** `harness.md:518` states the append-only context invariant: any insertion before the previous tail invalidates the provider KV cache and multiplies cost. So retrieved memory must arrive as a tail message at a turn boundary, never spliced mid-transcript, and never as a permanent system-prompt mutation (provider allowlisting favors a static system prompt — see `packages/coding-agent/CHANGELOG.md:3700`, "static system prompt for OpenAI allowlisting"). **[verified for the invariant; inferred for the consequence]**

**I5 — Retrieval quality is currently zero at the persistence layer.** Everything NAVI would need (`searchSessions`, `searchEntries`) is an interface with no implementation (`packages/agent/src/search/index.ts:20-27`; `post-wp05-roadmap.md:106`), and the only working matcher is a substring/fuzzy scan over an in-RAM concatenation (`session-selector-search.ts:116-154`). NAVI cannot "extend Pi search"; it must build one. **[verified]**

**I6 — NAVI's index will be the first component in this stack that needs retention.** Pi explicitly has none (`docs/session-format.md:13-17`, `docs/sessions.md:50`) and defers retention to an unbuilt partitioned-Postgres design (`harness.md:1251`, `post-wp05-roadmap.md:221`). Retention is therefore a NAVI-owned, must-ship-first responsibility, not an inherited capability. **[verified]**

**I7 — Two writers already exist by construction.** An ambient browser capture and the agent both want to write. Pi's precedent is unambiguous: the backend implements no lease, the host owns writable ownership (`sqlite-node/README.md:31`), and the one real lock is taken by the worker host (`session-worker.ts:533`). NAVI must name its writer roles explicitly rather than hoping SQLite settles it. **[verified for the precedent; inferred for the requirement]**

**I8 — The session relationship graph is a solved-looking shape.** `/resume` already builds a threaded tree from `parentSessionPath` (`session-selector.ts:206-231`) and the server already keeps a revisioned `SessionDirectory` (`experimental/services/sessions.ts:17-33`). NAVI's "tab relationships" and "session graph" are the same shape one level down (tabs/pages instead of sessions), so the UI/API pattern is proven even though the data does not exist. **[verified]**

---

## Recommended Approach

### A. Placement: what goes where

| Datum | Home | Why |
|---|---|---|
| Page visits, dwell/attention, tab lifecycle, embeddings, topics, cursors | **NAVI store** (own SQLite file, WAL) | High volume, needs TTL, needs rebuild, must not touch session semantics (I1) |
| "Remembered" items and selections | **NAVI store**, `created_by='remember'`, TTL-exempt | User-owned durable memory; survives sessions; must be deletable in one place |
| Per-conversation view of what was recalled | **Pi session**, `custom` entry, `customType:"navi.recall"` | Cheap, tiny (a list of page ids + scores), replayable on `session_start` per `docs/extensions.md:1477-1493`; **exclude from export** |
| The recall block actually given to the model | **Pi session**, `custom_message`, `customType:"navi.context"`, `display:true` | Keeps the transcript honest about what the model was told (`session-manager.ts:135-141`) |
| Explicit user instructions ("remember this repo") | **Pi session**, `custom` entry, `customType:"navi.pin"` | Pins are conversation-scoped intent; the *effect* (a promoted page/topic) lives in the NAVI store |
| Durable cross-conversation settings (site allow/deny, TTLs, model choice, incognito rules) | **NAVI store**, `config` table (or a small JSON file) | Not conversation state |
| Research-session naming/links | **Pi session** `session_info` for the conversation (via `pi.setSessionName`), **NAVI store** `sessions` row for the bundle | Both already have the right primitives: `session-manager.ts:1149-1175` and this design |

### B. Storage engine

**Recommendation: one SQLite database per NAVI profile (`~/.navi/memory/store.sqlite`), WAL, `busy_timeout=5000`, `BEGIN IMMEDIATE` for writers**, exactly mirroring the pragmas Pi already ships (`sqlite-node/src/sqlite/repo.ts`, `configureWritableConnection`). Three tiers of retrieval, each independently degradable:

1. **Structured filters first** (time window, domain, topic, tab/session, `content_state`, `created_by`) — this is what makes NAVI's "the repo I looked at earlier" work without any ML at all.
2. **Lexical scoring** — FTS5 (`bm25()`) **when the SQLite build has it**, else a `page_terms` fallback table queried by `term IN (...)`. *FTS5 must be optional*: Node's bundled `node:sqlite` is not reliably compiled with FTS5 (Windows builds ship without it) [external], and Pi's backend uses `node:sqlite` (`packages/session-backends/sqlite-node/src/index.ts:2`). Probe capability at open time and record it in `index_meta`.
3. **Vector scoring** — embeddings in a `BLOB` column, brute-force cosine over the **restricted candidate set only** (cap ≈5,000 rows), fused with the lexical ranking by reciprocal rank fusion (RRF, k=60).

Why brute force rather than a native vector index: at 384 dims, 5,000 candidates × 384 = 1.9M float ops ≈ a few ms in a JS typed array [inferred], while a full 90k-row scan (one year) is ~35M ops ≈ 30-80 ms and a 5-year 450k-row scan is ~170-400 ms — unacceptable for interactive recall. Candidate restriction, not an ANN library, is what makes this fast enough, and it avoids a native dependency (the repo pins direct deps exactly and gates install-lifecycle scripts, `AGENTS.md:44-49`).

**Embedding model:** local-first, `all-MiniLM-L6-v2` (384 dims) via Transformers.js/ONNX — ~22–29 MB download, CPU inference, no network [external]. API embeddings (`text-embedding-3-small`, 1536 dims, $0.02/1M input tokens [external]) as an explicit opt-in *only*, because embedding a browsing history sends a semantic profile of the user to a third party. Cost is not the deciding factor: at 500 pages/day × ~300 tokens ≈ 150k tokens/day, API embedding is ≈$0.003/day (≈$1.1/year) [inferred from external pricing] — **the decision is privacy, not money.** Record `model`, `dim`, `quant` on every vector row and refuse to score across mismatches.

### C. Schema sketch (SQLite)

```sql
-- ~/.navi/memory/store.sqlite   (WAL; busy_timeout=5000; BEGIN IMMEDIATE for writers)
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

CREATE TABLE profiles (
  profile_id  TEXT PRIMARY KEY,       -- browser profile / user profile
  created_at  INTEGER NOT NULL,
  policy_json TEXT NOT NULL);         -- TTLs, site allow/deny, capture watermark, embedding model

CREATE TABLE pages (                  -- one row per (profile, canonical url); the durable page record
  profile_id    TEXT NOT NULL,
  page_id       TEXT NOT NULL,        -- uuidv7, sortable by creation time
  url           TEXT NOT NULL,        -- as seen (redacted query params removed)
  url_key       TEXT NOT NULL,        -- canonicalized: host+path, tracking params stripped
  domain        TEXT NOT NULL,
  title         TEXT,
  first_seen    INTEGER NOT NULL,
  last_seen     INTEGER NOT NULL,
  visit_count   INTEGER NOT NULL DEFAULT 1,
  total_dwell_ms INTEGER NOT NULL DEFAULT 0,
  content_state TEXT NOT NULL,        -- 'none' | 'digest' | 'full'   (the privacy watermark)
  text_digest   TEXT,                 -- <=2 KB extractive summary; NULL when not captured
  lang          TEXT,
  promoted      INTEGER NOT NULL DEFAULT 0,  -- 1 = explicit "remember this"; TTL-exempt
  promoted_at   INTEGER,
  PRIMARY KEY (profile_id, page_id));
CREATE UNIQUE INDEX ix_pages_urlkey  ON pages(profile_id, url_key);
CREATE INDEX        ix_pages_lastseen ON pages(profile_id, last_seen DESC);

CREATE TABLE visits (                 -- append-only attention log (the raw ambient signal)
  profile_id  TEXT NOT NULL,
  visit_id    TEXT NOT NULL,
  page_id     TEXT NOT NULL,
  tab_id      TEXT,
  window_id   TEXT,
  session_id  TEXT,                   -- NAVI research session, nullable
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER,
  dwell_ms    INTEGER,
  attention   REAL,                   -- 0..1 (dwell, scroll depth, focus, foreground)
  created_by  TEXT NOT NULL,          -- 'observe' | 'remember' | 'import'
  PRIMARY KEY (profile_id, visit_id));
CREATE INDEX ix_visits_page_time ON visits(profile_id, page_id, started_at DESC);

CREATE TABLE selections (             -- user-highlighted snippets: explicit, high value
  profile_id   TEXT NOT NULL,
  selection_id TEXT NOT NULL,
  page_id      TEXT NOT NULL,
  text         TEXT NOT NULL,
  note         TEXT,
  tags         TEXT,                  -- JSON array
  created_at   INTEGER NOT NULL,
  ttl_days     INTEGER,               -- NULL = never expires
  PRIMARY KEY (profile_id, selection_id));
CREATE INDEX ix_selections_time ON selections(profile_id, created_at DESC);

CREATE TABLE topics (
  profile_id TEXT NOT NULL,
  topic_id   TEXT NOT NULL,
  label      TEXT NOT NULL,
  kind       TEXT NOT NULL,           -- 'topic' | 'entity' | 'project' | 'site'
  first_seen INTEGER NOT NULL,
  last_seen  INTEGER NOT NULL,
  weight     REAL NOT NULL DEFAULT 1.0,   -- decays; user-pinned topics are boosted
  pinned     INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (profile_id, topic_id));
CREATE UNIQUE INDEX ix_topics_label ON topics(profile_id, kind, label);

CREATE TABLE page_topics (            -- n:m assignment with provenance of the assignment
  profile_id TEXT NOT NULL,
  page_id    TEXT NOT NULL,
  topic_id   TEXT NOT NULL,
  score      REAL NOT NULL,
  source     TEXT NOT NULL,           -- 'url' | 'title' | 'body' | 'user' | 'llm'
  PRIMARY KEY (profile_id, page_id, topic_id));
CREATE INDEX ix_page_topics_topic ON page_topics(profile_id, topic_id, score DESC);

CREATE TABLE tabs (                   -- tab identity + lifecycle
  profile_id   TEXT NOT NULL,
  tab_id       TEXT NOT NULL,
  window_id    TEXT,
  opener_tab_id TEXT,
  first_seen   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL,
  state        TEXT NOT NULL,         -- 'open' | 'closed'
  PRIMARY KEY (profile_id, tab_id));

CREATE TABLE tab_edges (              -- the tab relationship graph (observed, not guessed)
  profile_id  TEXT NOT NULL,
  from_tab_id TEXT NOT NULL,
  to_tab_id   TEXT NOT NULL,
  kind        TEXT NOT NULL,          -- 'opener' | 'same_site_walk' | 'co_dwell' | 'co_topic'
  weight      REAL NOT NULL,
  last_seen   INTEGER NOT NULL,
  PRIMARY KEY (profile_id, from_tab_id, to_tab_id, kind));

CREATE TABLE sessions (               -- NAVI research sessions (distinct from pi sessions)
  profile_id   TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  title        TEXT,
  started_at   INTEGER NOT NULL,
  ended_at     INTEGER,
  pi_session_file TEXT,               -- link to ~/.pi/agent/sessions/.../*.jsonl when one exists
  PRIMARY KEY (profile_id, session_id));

CREATE TABLE session_pages (          -- which pages belong to which research session
  profile_id TEXT NOT NULL, session_id TEXT NOT NULL, page_id TEXT NOT NULL,
  weight     REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (profile_id, session_id, page_id));

CREATE TABLE embeddings (             -- one vector per (page, field); rebuildable
  profile_id TEXT NOT NULL,
  page_id    TEXT NOT NULL,
  field      TEXT NOT NULL,           -- 'title' | 'digest' | 'selection'
  model      TEXT NOT NULL,           -- e.g. 'Xenova/all-MiniLM-L6-v2'
  dim        INTEGER NOT NULL,
  quant      TEXT NOT NULL,           -- 'f32' | 'i8'
  vec        BLOB NOT NULL,           -- raw little-endian; i8 = dim bytes, f32 = 4*dim bytes
  norm       REAL NOT NULL,           -- precomputed L2 norm for cosine
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (profile_id, page_id, field));
CREATE INDEX ix_embeddings_model ON embeddings(model, dim, quant);

CREATE TABLE ingest_cursors (         -- S3-style pull-based catch-up (harness.md:574)
  source      TEXT NOT NULL,          -- 'browser' | 'pi-session'
  source_key  TEXT NOT NULL,          -- profile id / pi session id
  generation  INTEGER NOT NULL,
  cursor      TEXT NOT NULL,          -- opaque, monotonic (browser seq / entry seq)
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (source, source_key));

CREATE TABLE events_raw (             -- SHORT-LIVED queue only, not a log
  profile_id TEXT NOT NULL, seq INTEGER NOT NULL,
  kind TEXT NOT NULL, payload TEXT NOT NULL, received_at INTEGER NOT NULL,
  PRIMARY KEY (profile_id, seq));

CREATE TABLE index_meta (             -- capability + rebuild bookkeeping
  key TEXT PRIMARY KEY, value TEXT NOT NULL);
  -- rows: schema_gen, fts5('1'|'0'), embed_model, embed_dim, corpus_gen, last_sweep

-- OPTIONAL accelerator, created ONLY when the SQLite build reports FTS5:
CREATE VIRTUAL TABLE pages_fts USING fts5(
  title, url, text_digest,
  content='pages', content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2');

-- FTS5-less fallback:
CREATE TABLE page_terms (
  profile_id TEXT NOT NULL, page_id TEXT NOT NULL, term TEXT NOT NULL,
  tf INTEGER NOT NULL, field TEXT NOT NULL,
  PRIMARY KEY (profile_id, page_id, term, field));
CREATE INDEX ix_page_terms ON page_terms(profile_id, term);
```

**Append-only JSONL alternative (for comparison), one record per line, same logical model:**

```json
{"v":1,"type":"visit","id":"0192...","at":1757600120000,"tab":"t7","page":{"url":"https://...","title":"..."},"dwellMs":12400,"attention":0.71,"cause":"observe"}
{"v":1,"type":"page_digest","id":"0192...","at":1757600120100,"pageId":"0192...","digest":"...","digestModel":"extractive-v1"}
{"v":1,"type":"remember","id":"0192...","at":1757600300000,"target":"page:0192...","note":"the repo I'll need","tags":["rust","async"]}
{"v":1,"type":"forget","id":"0192...","at":1757600400000,"scope":{"domain":"example.com"}}
{"v":1,"type":"topic_link","id":"0192...","at":1757600300100,"pageId":"0192...","topic":"rust async","score":0.62,"source":"title"}
```

### D. Ingest points

**Browser side (the primary source), normalized before it reaches storage.** Do *not* persist raw DOM/scroll/network events; reduce them at the edge:
- `tab_created` / `tab_activated` / `tab_moved` / `tab_closed` → `tabs`, `tab_edges` (kind `opener`).
- `navigation_committed` (URL change, main-frame) → new `pages` candidate; canonicalize URL, apply the tracking-param strip list, match to `url_key`.
- `content_loaded` (title + readable main text) → compute `text_digest` **only if** the page policy allows capture (`content_state`), else leave `'none'`.
- `dwell_tick` (aggregated, e.g. 5 s buckets, foreground only) → accumulate into the open `visits` row; flush on blur/navigate/close.
- `selection` / `clip` / explicit "remember" gesture → `selections` + `visit.created_by='remember'` + promote the page.
- `window_incognito` → **suppress all ingest** for that window (asserted, not inferred).
- Never: keystrokes, form values, cookies, request/response bodies, full page HTML.

**Agent side (Pi-shaped, verified event names from `docs/extensions.md:277-349`):**
- `session_start` (reasons `startup|reload|new|resume|fork`) → open store, run the bounded catch-up sweep, replay `navi.recall` / `navi.pin` custom entries to rebuild the per-conversation view, publish a capability row in `index_meta`. Deferred here (not in the factory) per `docs/extensions.md:222-224`.
- `input` → detect explicit intents ("remember this", "what was that site…", "the repo I looked at earlier"); on "remember", call the promote path and mark the turn as memory-bearing.
- `before_agent_start` → optional *proactive* recall block for a fresh turn, injected as a `custom_message` (tail) with a strict token budget.
- `context` (before each LLM call) → **the injection point** (`docs/extensions.md:675-685`): append the recall block at the tail only (`harness.md:518`), or return `{messages}` unchanged when the budget is exhausted.
- `agent_settled` / `session_shutdown` → flush pending writes, advance `ingest_cursors`, checkpoint `corpus_gen`.
- `session_before_switch` / `session_before_fork` / `session_tree` → write the `sessions.pi_session_file` link and the session→page association for the current research thread.
- `tool_result` on a `navi_*` tool → the tool's `details` carry ids and scores so branch/fork copies keep provenance (the repo's own pattern for stateful tools, `docs/extensions.md:1885-1917`).

### E. Retrieval APIs exposed to the agent

Design targets (NAVI APIs — proposals, not repo APIs; the *registration* mechanism is `pi.registerTool`, `docs/extensions.md:1385-1420`):

```
navi_recall({ query, mode?: "hybrid"|"keyword"|"meaning",
              kind?: "page"|"selection"|"topic"|"session",
              since?, until?, domain?, tabId?, topic?, onlyRemembered?, limit? })
  -> { content: [ text block with provenance lines ],
       details: { hits: [{ pageId, url, title, domain, when, snippet, score,
                           matchedBy: ["time","domain","topic","keyword","vector"],
                           contentState }],
                  degraded: { fts5: false, vectors: true } } }

navi_remember({ target: {pageId | url | selectionId}, note?, tags?, ttlDays? })
  -> { content: [...], details: { pageId, promoted: true, ttlDays } }

navi_session_recap({ session?: "current"|sessionId, window?: "last-hour"|"today"|"thread" })
  -> { content: [...], details: { pages: [...], topics: [...] } }

navi_forget({ scope: { pageId? | domain? | tag? | since? | selectionId? }, dryRun? })
  -> { content: [...], details: { deletedPages, deletedVisits, deletedVectors, rebuilt: true } }
```

Internal (not model-callable), used by the harness hooks:
`Recall.materialize(query, budgetTokens) -> RecallBlock`, `Recall.note(...)`, `Recall.sweep(retentionPolicy)`, `Recall.catchUp(source)`.

**Tool-call count discipline:** `navi_recall` is available for *deliberate* lookup, but the ambient path must be push-based at turn boundaries. Requiring a tool round-trip per event would both add latency and (worse) fragment context — and mid-transcript insertion violates the append-only context invariant (`harness.md:518`).

### F. Retrieval pipeline (recommended)

```
structured filter (time, domain, topic, tab/session, content_state, created_by)
  -> lexical rank (FTS5 bm25 | page_terms)
  -> vector rank (cosine, i8 or f32, candidate cap ~5k)
  -> RRF fuse (k=60)
  -> temporal decay: score *= exp(-Δdays/τ),  τ = 14d for observed, ∞ for remembered/pinned
  -> diversity: per-domain cap (≤3), MMR λ≈0.7
  -> bundle: top-k, per-item truncation (200-400 tokens), total budget 1-2k tokens
  -> emit with provenance ("you visited <domain> 2 days ago — recalled, unverified")
  -> fail closed: store unavailable ⇒ return nothing and say "no memory available"
```

### G. Retention, pruning, and rebuild policy

```sql
-- nightly sweep (single writer, inside the writer role)
DELETE FROM visits      WHERE ended_at IS NOT NULL AND ended_at < now - visits_ttl_ms;
DELETE FROM pages       WHERE last_seen < now - pages_ttl_ms
                          AND promoted = 0
                          AND NOT EXISTS (SELECT 1 FROM selections s WHERE s.page_id = pages.page_id);
UPDATE pages SET text_digest = NULL, content_state = 'none'
                          WHERE last_seen < now - digest_ttl_ms
                            AND content_state = 'digest' AND promoted = 0;
DELETE FROM embeddings  WHERE page_id NOT IN (SELECT page_id FROM pages WHERE pages.profile_id = embeddings.profile_id);
DELETE FROM topics      WHERE weight < topic_weight_floor AND last_seen < now - topics_ttl_ms AND pinned = 0;
DELETE FROM events_raw  WHERE received_at < now - 24h;   -- the queue is never a log
-- monthly: PRAGMA incremental_vacuum / VACUUM after a sweep, off the interactive path
```

**Retention table** (defaults for a single heavy browser user; volumes are per-day estimates — see the arithmetic note below):

| Class | Example | Default TTL | Est. rows/day | Est. size/day | Reclamation mechanism | Privacy weight |
|---|---|---|---|---|---|---|
| Raw browser events | scroll/DOM/focus deltas | **not persisted** (≤24 h in `events_raw`) | 0 (queue drains) | ~7.5 MB if wrongly logged; 0 by design | drain at ingest | none stored |
| Visits (attention log) | 12 s dwell on a docs page | 30–90 d | ~250 | ~60 KB | TTL sweep | medium (URL + time) |
| Pages (metadata) | url, host, title, counters | 12 mo rolling (12 mo min for "remembered") | ~150 new | ~360 KB | TTL sweep | medium-high |
| Text digests | ≤2 KB extract | 90 d, and only where capture is allowed | ~150 | ~300 KB | NULL the column, keep metadata | high |
| Selections / quotes | user-highlighted snippet | indefinite (explicit) | 5–20 | <20 KB | user-only delete | high (intentional) |
| Embeddings | 384-d per page | bound to page TTL | ~250 | ~375 KB (f32) / ~100 KB (i8) | recompute-on-sweep; drop with page | derived, high |
| Topics / entities | "rust async", "anthropic" | 12 mo, weight-decayed | tens of upserts | <20 KB | low-weight prune | high (aggregated interests) |
| Tab graph | opener / co-dwell edges | 30 d | ~300 edges | ~60 KB | TTL sweep | low-medium |
| Research sessions | named bundles | indefinite (explicit) | 1–3 | <10 KB | user-only delete | high |
| NAVI entries in pi sessions | `navi.recall`, `navi.pin`, `navi.context` | follows the pi session | <1 KB/session | — | follows session deletion; **excluded from export** | high (leave the machine via `/share` if not excluded) |

**Arithmetic note (my assumptions, [inferred]):** 250 page-attention events/day ≈ 4 h of active browsing at ~1 page/min; 150 of those are pages not seen before. Row sizes: visit ≈ 240 B with SQLite overhead (two uuid-sized ids, tab/session ids, four integers, one real, a small enum); page ≈ 370 B metadata + 2 KB digest; vector = 384 B (i8) or 1,536 B (f32). => metadata+visits+vectors ≈ **0.5–0.8 MB/day, ≈180–290 MB/year**. If every page were stored as full text (10 KB), add 2.5 MB/day → **≈1.9 GB/year**. If every browser event were logged (~200 events/page × 250 pages = 50k events at ~150 B) → **≈7.5 MB/day, ≈2.7 GB/year**. Content policy, not metadata, decides the storage bill by a factor of ~7.

**Index rebuild cost:**
- Local (all-MiniLM-L6-v2, CPU): ~10–20 ms/page [external model size ~22–29 MB; inference time my estimate] → 250 pages/day ≈ 3–5 s of background CPU; a full 1-year rebuild (≈90k pages) ≈ **15–30 min of CPU** and ≈130 MB of vectors recomputed. Run at idle (`agent_settled`) or nightly; never inside an event handler.
- API (`text-embedding-3-small`): 90k pages × ~300 tokens = 27M tokens ⇒ **≈$0.54 per full rebuild** at $0.02/1M [external pricing] — cheap, but it means re-sending the corpus to a third party, which is the actual objection.
- Retrieval latency: structured + FTS5 + 5k candidates × 384 dims ≈ a few ms [inferred]; an unrestricted 90k-row vector scan ≈ 30–80 ms and a 450k-row scan ≈ 170–400 ms [inferred] — the reason candidate restriction is non-negotiable.

### H. "Remember this": explicit vs implicit

**Recommendation: implicit observation + explicit promotion, with a per-site capture watermark.**
- **Implicit (always):** domain, URL, title, timestamps, dwell/attention, tab/session linkage. No page text. This is enough for "the repo I looked at earlier" when the user names the repo, because the *user's own words* carry the semantics and the store only needs to resolve them (structured filter + keyword).
- **Implicit-content (per-site opt-in or global opt-in):** `content_state='digest'` — a ≤2 KB extractive digest of the main text, produced on-device. This is the switch that turns NAVI from "browsing log" into "semantic browsing memory", and it is exactly the switch that should require consent.
- **Explicit (`remember this`, highlighting, clipping):** promote to durable, TTL-exempt, embed with the higher-quality (and optional API) model, and tag. Promoted items are the only ones allowed to be surfaced without a query mentioning them.
- **Never implicit:** screenshots, form data, cookies, request bodies, private/incognito windows, password-manager pages, banking/health domains (default deny list).

Rationale that is repo-grounded rather than taste: Pi's own model makes the *projection* step explicit and separate from storage (`custom` vs `custom_message`, `session-manager.ts:104-141`; `sessionEntryToContextMessages` 383-408), and the roadmap's S3 design insists the index "is a rebuildable projection with zero authority" (`harness.md:574`). Ambient capture + explicit promotion is the same discipline applied one layer out.

---

## Alternatives

**A1 — Everything in Pi session `custom` entries.** Rejected. Breaks on immutability (`session-manager.ts:1315-1317`), undelible growth (J1 unlanded, `harness.md:57`), per-open RAM/parse cost (`loadEntriesFromFile` 514-557; `buildSessionInfo` 688-766), tree pollution (`_appendEntry` 1058-1063), and `/share` upload (`session-share.ts:107-120`). Viable *only* for the tiny per-conversation slice recommended in A.

**A2 — Append-only NAVI JSONL event log + rebuildable in-RAM index.** Attractive because it mirrors Pi's own format (crash-safety story verified in `harness.md:356-368`) and the S3 cursor design. Rejected as the *primary* store because: JSONL open replays the entire log into RAM (`harness.md:361`), queries then run in RAM, and it needs its own snapshot compaction — which the very same docs say is specified-but-unimplemented (J1). Choosing A2 means writing the compaction machinery Pi never shipped. Keep A2 as the **export/inspection format** and as a disaster-recovery backup, not as the serving store.

**A3 — SQLite + FTS5 + brute-force vectors over restricted candidates.** **Recommended.** Reuses the pragma/transaction patterns Pi already ships (`sqlite-node/src/sqlite/repo.ts`), needs no native dependency, degrades gracefully when FTS5 is absent, and is a dead-simple rebuildable projection.

**A4 — SQLite + a native vector index (`sqlite-vec`, hnswlib, FAISS).** Better asymptotics beyond ~1M vectors, and worth revisiting then. Rejected for v1: a native module adds install/build surface (`AGENTS.md:44-49` gates lifecycle scripts and requires review), and the brute-force cost at NAVI's realistic scale is already acceptable once candidates are restricted.

**A5 — Hosted/shared vector DB (Qdrant, LanceDB cloud, pgvector).** Rejected for v1: it is a semantic browsing history leaving the device, and it forces a retention/partitioning decision Pi explicitly deferred (`harness.md:1251`, `post-wp05-roadmap.md:221`). Acceptable later for an org-managed deployment with an explicit DPA.

**A6 — No store; reuse Pi's existing session search.** Not available: `SessionSearchService` has no implementation (`packages/agent/src/search/index.ts:20-27`; `post-wp05-roadmap.md:106`), and the shipped matcher is substring/fuzzy over an in-RAM string (`session-selector-search.ts:116-154`). Also note the roadmap's warning that post-filtering after a ranked `limit` is **unsound** (`post-wp05-roadmap.md:110`) — any NAVI design that clones sessions into one blob and filters afterwards inherits that bug.

**A7 — Per-event LLM calls (summarize/extract as you browse).** Rejected: cost and latency scale with events, not with turns; and it is the exact write-amplification anti-pattern the repo demanded measurements for (`post-wp05-roadmap.md:160-170`). Batch LLM extraction at idle, and only for promoted or high-attention items.

**A8 — Persistent system-prompt memory.** Rejected: `harness.md:518` (append-only context invariant) and the provider-allowlisting sensitivity of a static system prompt (`CHANGELOG.md:3700`). Use tail-injected `custom_message` blocks with an explicit budget.

---

## Risks

1. **Privacy / re-identification.** A semantic browsing history is worse than a URL history: it is retrievable *by theme*, so it reconstructs interests, health/finance/legal activity, and political leanings even if the exact URLs are never shown. Mitigations: no text capture by default; deny-list of sensitive domains; incognito suppression asserted at ingest; local-only embeddings by default; store file mode 0600 in a dedicated directory (Pi's precedent for lock-protected local stores: `core/auth-storage.ts:76,128`, `core/settings-manager.ts:243`, `core/trust-manager.ts:146`).
2. **Export leakage via `/share`.** `session-share.ts:26-43` appends extension data to the shared ndjson and uploads it with `visibility=organization` (Radius) or as a gist. Any NAVI `custom`/`custom_message` entry containing page URLs, titles, or topics goes with it. Mitigation: an explicit export filter for `customType` prefix `navi.`, plus a "share without memory" default and a warning when a shared session contains recalled memory.
3. **FTS5 may not exist.** Node's bundled `node:sqlite` is not reliably compiled with FTS5 (Windows builds ship without it) [external], and NAVI's likely host uses exactly that module. Mitigation: capability probe + `page_terms` fallback + surface `degraded.fts5` in tool results so the model can say "keyword-only recall".
4. **Unbounded growth with no reclamation.** Pi proved this failure mode on itself and has not fixed it yet (`harness.md:57`). If NAVI ships capture without a sweep, the store becomes a liability. Mitigation: retention ships in the same milestone as capture; the sweep is the *first* writer to be tested.
5. **Ranking pathologies.** Recency dominance (yesterday's noise beating last week's important page), single-domain flooding, and entity confusion ("Mercury" the planet vs the database). Mitigations: temporal decay with an explicit floor for remembered items, per-domain caps, topic kinds (`entity` vs `topic` vs `project`) with pinned synonyms, and always showing `when` + snippet so the model can disbelieve the hit.
6. **Stale or wrong memory presented as fact.** Sites change; URL canonicalization merges distinct pages; a one-off visit becomes a "topic"; "the repo I looked at earlier" resolves to the wrong repo. This is the highest-harm failure because the model will speak confidently. Mitigations: label every injected memory as *recalled and unverified*; require `when` + source URL in the injected block; purge digests aggressively (90 d) while keeping metadata; prefer "ask the user to confirm" over silent action on high-stakes recall.
7. **Model/dimension drift in vectors.** Scoring a query encoded with model B against vectors from model A yields plausible-looking garbage. Mitigations: `model`,`dim`,`quant` columns; refuse cross-model scoring; `corpus_gen` in `index_meta` bumped on model change triggers a background rebuild and reports `degraded` until it finishes.
8. **Writer concurrency.** Browser helper + agent + sweeper. Pi's precedent says the backend will not arbitrate (`sqlite-node/README.md:31`). Mitigations: one designated writer role; WAL + `busy_timeout` + `BEGIN IMMEDIATE` for safety; readers use read-only connections (the pattern Pi uses for listing/foreign fork sources).
9. **Cold-start cost inside a hook.** Store open + catch-up + possible rebuild on `session_start` must be bounded. Mitigation: a strict time budget for catch-up, deferred heavy work to idle, and the repo's own rule that the extension factory must not start long-lived resources (`docs/extensions.md:222-224`).
10. **Retention vs user expectation.** "Where did yesterday's tab go?" Sweeps that surprise the user destroy trust. Mitigation: TTL-visible defaults, a "keep this" affordance, and a `/navi status` that reports what is stored and when it expires.
11. **Compliance.** Browsing data may include regulated content, and a plaintext store on disk is discoverable. Mitigation: document plaintext, offer site-level exclusion and encryption-at-rest via an OS-keychain-derived key as an opt-in, and make `navi_forget` actually rebuild the index (it must not leave derived rows behind).
12. **Index drift vs the session log.** If NAVI memory links to a Pi session whose entry ids later change (a real hazard: Pi renumbers `seq` on the precise rewrite, `harness.md:574`), links dangle. Mitigation: store session **file path + entry id**, not `seq`; treat missing ids as "expired reference" rather than an error; key NAVI cursors on `(source, generation)` exactly as S3 specifies.

---

## Feature Assessment

| Concept | Verdict | Rationale |
|---|---|---|
| Semantic browsing history (retrieval by meaning, not URL) | **CORE** | The product's premise; requires the store + embeddings; nothing in Pi provides it (`search/index.ts` is a skeleton) |
| Recent-context memory (working set, last hours/day) | **CORE** | Cheapest high-value tier: pure structured filter + recency, no ML, no consent question for metadata-only capture |
| Remembered pages (canonical page records) | **CORE** | The unit everything else references; `url_key` canonicalization is the one thing that must be right from day one |
| `remember this` as an explicit user action | **CORE** | The consent boundary and the only zero-risk promotion path; maps cleanly onto Pi's `custom`/`custom_message` split |
| Retrieval API exposed to the agent (`navi_recall`) | **CORE** | Without a callable surface the memory is inert; `pi.registerTool` is the proven mechanism |
| Temporal context ("the repo I looked at earlier") | **CORE** | This is what structured filtering buys; do not solve it with embeddings alone |
| Topic/entity memory | **LIKELY** | High value for ambient context, but entity resolution quality (merging "Mercury") is the risk; ship deterministic extraction first |
| Tab relationships (opener / same-site walk / co-dwell) | **LIKELY** | Pi already ships the analogous session-lineage tree (`session-selector.ts:206-231`), so the shape is proven; keep the edge set small and observed |
| Research sessions (named bundles linking pages + a pi session) | **LIKELY** | `pi.setSessionName` + a NAVI `sessions` row already cover the primitives; deliver as glue, not as a new subsystem |
| Remembered elements (DOM-level nodes/highlights) | **EXPERIMENTAL** | Very high precision but brittle selectors, high re-identification risk, and no repo precedent; pilot behind explicit opt-in |
| Session graph across tabs + conversations | **EXPERIMENTAL** | `SessionDirectory` (`experimental/services/sessions.ts:17-33`) proves the shape but not the value; needs a real UI consumer first |
| Implicit full-text capture of every page | **DEFER** | 7× the storage and the whole privacy surface; make it per-site opt-in, never the default |
| Screenshot / visual memory | **DEFER** | Large bytes, highest sensitivity, weakest retrieval story |
| Cross-device or cross-browser memory sync | **DEFER** | Forces the hosted-store decision (A5) that is currently rejected |
| Hosted vector DB or API embeddings by default | **REJECT** | Ships a semantic browsing profile to a third party to save ~$1/year |
| Storing browsing memory inside pi session JSONL | **REJECT** | Immutable, unreclaimable, parsed on every open, tree-polluting, and uploaded by `/share` |
| Eager background embedding inside the extension factory | **REJECT** | Violates the repo's own rule (`docs/extensions.md:222-224`) and creates per-event write amplification |
| Unbudgeted proactive memory injection every turn | **REJECT** | Violates the append-only context invariant (`harness.md:518`) and inflates cost silently |
| Persistent memory written into the system prompt | **REJECT** | Cache/allowlisting cost (static-prompt sensitivity, `CHANGELOG.md:3700`); use tail messages instead |
| A "precise rewrite"/erasure implemented as a runtime mechanism | **DEFER** | The repo deliberately keeps it as administrative tooling (`harness.md:578-582`); NAVI should implement `navi_forget` as *store-side delete + rebuild*, not as an in-log rewrite |

---

## Open Questions

1. **Where does browsing capture physically run?** A browser extension/helper writing to the NAVI SQLite file, or a local service the agent queries? This decides the writer-role design (I7). *Confidence that it must be decided before schema freeze: high.*
2. **Is `content_state='digest'` opt-in per site, or global-with-deny-list?** Per-site is safer and more work; global is simpler and scarier. *Need a product call; I lean per-site with a global "off" default.*
3. **Does NAVI own a local embedding model, or can it borrow an existing provider path?** Pi resolves auth through `ctx.modelRegistry`/`auth-storage`; a local ONNX model is a new dependency surface (Transformers.js, not currently in the repo). *Open; medium confidence that local-first is right.*
4. **What is the token budget for auto-injected recall?** 1k tokens is my default guess; it interacts with `keepRecentTokens`/`reserveTokens` (compaction defaults 20k/16k, `docs/compaction.md:35,43`). *Unknown; needs measurement against real prompts.*
5. **Do NAVI research sessions map 1:1 onto pi sessions, or many-to-one?** A single pi session may traverse several research threads, and `/fork` duplicates a session. *Open.*
6. **Should recalled memory appear in `/share` exports at all?** Privacy says never; reproducibility says the transcript should show what the model was told. *I favour: show the fact of recall, redact the contents.*
7. **How is retention surfaced without nagging?** *Open; needs UX work.*
8. **Does NAVI need the store to be multi-profile?** The schema above is profile-scoped from day one because retrofitting a scope column is a rewrite. *Confidence: high that scoping early is cheaper.*
9. **What happens when the store is fresh (day 1) or after a wipe?** Recall must degrade to "nothing remembered yet", never to a hallucinated recollection. *Design requirement; confidence high.*
10. **Is there any NAVI need that Pi's `value`/`list` addresses should serve instead?** `values.md:5-20` provides typed durable addresses and explicitly says applications own their cleanup and fork policy; a small per-session NAVI slice could live there rather than in the session entry tree. *Open; a plausible simplification, but only if NAVI is built inside the AgentHarness storage model.*

---

## External sources

- FTS5 not reliably compiled into Node's bundled `node:sqlite` (issue reports for Node 22+/Windows builds): <https://github.com/openclaw/openclaw/issues/20987>, <https://github.com/skilld-dev/skilld/issues/64>, <https://sqlite.org/fts5.html>
- `sentence-transformers/all-MiniLM-L6-v2` — 384-dimensional sentence embeddings; Transformers.js/ONNX build downloads ~21.8 MB (uint8/WASM) to ~28.6 MB (q4f16/WebGPU): <https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2>, <https://huggingface.co/Xenova/all-MiniLM-L6-v2>, <https://localmodel.run/browser/all-minilm-l6-v2>
- OpenAI `text-embedding-3-small` — 1536 dimensions, 8,191-token max input, $0.02 per 1M input tokens: <https://developers.openai.com/api/docs/models/text-embedding-3-small>, <https://openrouter.ai/openai/text-embedding-3-small>
