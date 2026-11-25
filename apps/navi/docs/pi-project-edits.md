# Pi edit proposals, preparation only

`workers/project_edits.py` is a pure proposal builder. It cannot read files,
apply a patch, write, invoke commands, approve itself or call a provider. No
production route, model tool, worker control or Station operation imports it.

The future owner bridge supplies a complete already-approved UTF-8 snapshot
and its SHA256 digest. A caller proposes an exact replacement for one relative
text/code path. The builder checks digest, path and content bounds and creates
an immutable before/after binding. The review includes exact before/after
strings and hashes, not only a potentially ambiguous unified diff. Diff output
is a visual aid, never executable patch text. No-op changes, binary/directional
controls, invalid paths, overlong text and stale snapshots are refused.

Bounds: 16 KiB and 1000 lines per before/after, 64 KiB diff, project-read path
and extension policy. Eleven synthetic tests pass. No project bytes were read
or changed by these tests beyond in-memory strings.

## Before an apply operation can exist

- The owner-selected project and current turn must own the snapshot, review,
  approval and exact proposal digest. No model-selected roots or review IDs.
- Show the actual file path and full reviewed changes, including EOF state.
  Approval means this exact edit once, not a later replacement or another file.
- Refuse a changed/replaced file, path or root. A digest check followed by a
  normal write or rename is not atomic compare-and-swap. An advisory flock
  cannot constrain arbitrary editors. Do not claim otherwise.
- Resolve commit-versus-cancel ordering before enabling writes. If commit may
  have happened without confirmation, report an unknown outcome and block
  automatic retry. Cancellation cannot undo a completed write.
- Rollback cannot overwrite a later human edit. It needs exact current-state
  binding and its own safe route. Never promise automatic rollback merely
  because a backup exists.
- No shell capability through an edit. No creating or deleting files, changing
  modes, following links or modifying multiple files in this first scope.
- Test the real owner UI and transport, concurrent file changes, cancel before
  and during commit, process death and duplicate decisions before exposure.

These are remaining implementation gates, not claims of existing protection
for a write path. Pi project remains read-only and OpenCode stays default.

## Preparatory review lifecycle

`project_edit_reviews.py` now binds one pending review to an owner scope, random
one-shot ID, exact proposal digest and deadline. It revalidates a proposal's
before/after hashes and diff before presenting it. Wrong scope, ID, digest,
Always and apply decisions are refused. A stale snapshot or timeout consumes
the review. Close clears it; four reviews maximum. Mutating the presented copy
cannot change the stored proposal. Eleven additional synthetic tests pass.

The only choices are accept_proposal/reject. Acceptance means a proposal may be
exported manually, not permission to write. Every result says applied:false.
There is no production caller, model tool, transport, UI or filesystem method
for this preparatory state machine. The caller-supplied current digest is only
as trustworthy as the future owner bridge. It does not establish an atomic
commit, current filesystem identity or write authority.

## Full snapshot identity

Approved project reads now include snapshot_sha256/snapshot_bytes only when the
returned bytes are the complete file beginning at line one, not clipped or
line-truncated. Empty files qualify. Partial reads never carry a whole-file
digest. The TS model adapter independently verifies the digest against the
exact returned UTF-8 bytes before passing snapshot metadata through. This
provides a proposal basis, not permission or an atomic write guarantee.

Thirteen reader tests, 43 runtime tests and three type checks pass, and all
fourteen actual project-daemon tests pass after the snapshot addition. No edit
apply operation was enabled.

## Optional previews, never apply

The TS project loop has an optional allowPreviews flag, default false. It may
keep at most four complete already-approved snapshots in run-local memory and
prepare one exact replacement preview per path against that snapshot digest.
The model cannot supply the base snapshot or acquire another read through the
preview tool. Previews are returned only after a completed run and cleared on
finish; canceled runs return none. No write bridge is called. The production CLI enables previews only when
NAVI_PROJECT_PREVIEWS=1, separately from the approved-read opt-in. Station
shows exact before/after strings, hashes, UTF-8 lengths and EOF newline state.
It says no file changed and offers no apply button. Preview content is not
saved in Station message history. It is still present in the live turn event
stream for that process, like other tool results.

Eight preview-boundary tests and one injected-loop test pass, bringing runtime
tests to 52 plus three type checks. Tests cover no snapshot, partial/forged
snapshots, control fields, wrong digest, bounds, deceptive controls, duplicate
paths, immutable copies and Unicode. Fifteen actual daemon tests now include approved read through model preview
and final Station reply, exactly one owner-read approval and unchanged file.
Actual Chromium renders both sides and treats markup as text. There is no apply operation.

Four more loop tests cover disabled preview capability, cancellation after a
prepared preview, incomplete turn-budget runs and no snapshot carryover across
runs. Runtime57 and typechecks3 pass. Rust55 passed after the UI integration.

Preview bases now reject deceptive controls and oversized line counts too,
not only proposed replacements. An additional boundary test covers both.

## Manual proposal export

The preview offers Copy proposal JSON. Only an explicit owner click copies the
exact strings, digests and path, with applied:false and
requires_fresh_validation:true. This is data, not an executable patch or shell
command. Copying does not approve or apply anything and never calls a daemon
operation. The destination is the local clipboard, not a network recipient.
Actual Chromium verifies no automatic copy, exact bytes including EOF, no
integration call on click and no preview content in message_save.

## Preparatory private review workspace

project_workspace.py can create a fresh private review directory in an
owner-private storage root. It writes exact before.txt, proposed-after.txt and
review.json with hashes and original path metadata. The path never becomes a
filesystem destination. Files are non-executable inert .txt, not project code
files; no import, apply, shell, project root or model route exists. One unique
review per call, exclusive file creation, 0700 directory/0600 files, component
no-symlink opens and unsafe-ancestor refusal.

Nine actual filesystem tests cover exact content/modes, no path materialization,
unique calls, symlink/nonprivate/unsafe roots and cancellation/write cleanup.
Unchanged project files are not claimed as an applied edit. A lost delivery
after creation can leave a private review workspace; do not silently retry.
Production transport, durable indexing/cleanup/recovery and owner UI remain
implementation gates. Abrupt process death may leave partial review files;
these must never be treated as complete without manifest/hash verification.

## Review workspace recovery checks

A fixed review-ID inspector reopens only the three expected files through
no-symlink descriptors. Owner/modes/type/single-link/size are checked, FIFOs are
nonblocking and refused, changed reads rejected, duplicate manifest keys refused,
and both content hashes plus the exact proposal digest reconstructed. Partial,
extra, corrupt or changed review workspaces never count as complete. Seventeen
filesystem tests pass.

This proves internal snapshot consistency only, not authorization, authenticity
against another process running as the same user or current source-file state.
It never reads the original project file. Automatic cleanup/deletion remains
unimplemented: no generic recursive deletion was introduced. Stored original
paths remain metadata. Production durable index/retry/recovery/UI are still open.

## Idempotent review creation transport

An owner-generated 128-bit hexadecimal request ID deterministically selects one
review directory. Retrying that ID recovers only a complete hash-consistent
matching proposal. Changed proposal, partial state or collision blocks rather
than overwriting or allocating a second directory. No content-derived ID is
used. The single-request helper accepts request_id/preview only; owner storage
is a process argument, never a payload field. Shared exact-preview decoding
revalidates the same bytes as syntax validation. Twenty-four workspace tests
include actual isolated-Python CLI create/recover; twenty-two validator tests
pass after the decoder extraction. No daemon/UI route yet.

Do not delete incomplete state automatically or generate a fresh request ID on
ambiguous failure. A complete manifest can be recovered; an incomplete workspace
needs explicit owner cleanup. Idempotency does not prove ownership/permission.

## Optional owner storage route, no UI yet

NAVI_PROJECT_WORKSPACES=1 separately enables owner-only pi_preview_workspace.
Default off, no model tool. It accepts explicit selected project, request ID and
exact preview only. Fixed isolated-Python helper receives cleared environment
and the owner data path, creates proposal-reviews beneath verified private data,
never a payload storage path. The daemon checks generation/project epoch/owner
socket liveness, has one-active/six-second bounds and stops the child.

A canceled/disconnected/failed reply after creation can be ambiguous. Recover
with the same request ID; never allocate a fresh one automatically. Partial
state blocks with manual-cleanup caveat. Five actual daemon tests cover create,
same-ID recovery, changed content, extra paths/commands, partial state and default
off. No Station workspace button yet; in-flight race acceptance remains open.
The storage path returned is private local data, not a shared or uploaded File.

## Owner UI and lost-reply acceptance

Station exposes Create or recover private review files beside the exact preview.
It generates one cryptographic request ID and reuses it for subsequent clicks
on that preview. No request happens automatically. Original project selection
is required. It displays the request ID, local storage path and no-project-change
status; unconfirmed replies explicitly require the same-ID recovery, never a
fresh request. Preview/runtime state is not saved to chat history, so after a UI
reload use the displayed request ID and existing storage rather than rebuilding
a fresh preview/request blindly. Durable workspace browsing remains open.

Eight actual owner-daemon tests include real complete creation followed by a
delayed reply: Stop, project switch and owner EOF reap the helper and suppress
the reply; same-ID recovery after restart finds exactly one matching workspace.
Chromium checks explicit clicks, original project/preview binding and same-ID
reuse, and the rendered recovery status was inspected. No project apply or
automatic deletion.

## Reload-safe request metadata

Before any workspace request, Station saves at most 32 metadata bindings
(project, relative path, before/after hashes, request ID), no snapshot content,
in local app storage. Repeating an exact preview after reload reuses that ID.
Unreadable/invalid/full/unwritable metadata blocks a new request, never silently
resets or evicts old IDs. Chromium verifies same-ID reuse across an actual reload
and absence of source text in the metadata. Existing workspace files are still
private local data, not automatic shares.

A Web Locks lock serializes metadata allocation across same-origin Station
windows. Missing lock support blocks creation rather than risking duplicate IDs.
This covers one storage profile, not loss of the profile or separate profiles.
Do not claim global deduplication.
Durable inventory/recovery by review ID is still open. Do not clear metadata or
retry with a new ID after an ambiguous create; retain it for manual recovery.

Actual Chromium same-origin concurrent windows now verify Web Locks allocate
one ID, plus invalid/full/unwritable metadata fail closed. Separate profiles
and cleared storage still require manual recovery, not duplicate-proof claims.
