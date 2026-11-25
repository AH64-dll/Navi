# Navi parity and acceptance, November 24, 2026

This matrix is an implementation/acceptance record, not a completion percentage.
OpenCode remains the default. Optional routes do not claim OpenCode parity.

| Area | Implemented | Evidence | Still open |
| --- | --- | --- | --- |
| Native Pi text | Bounded tool-free native RPC, explicit provider/model, private attachment snapshots, optional project/model session scope | 35 RPC tests, daemon Pi tests, clean patched-tree runs | Owner-selected real provider/device acceptance |
| Pi browser | Local injected-provider loop; explicit document capability grant; observe/focus/scroll; exact one-shot effect approval | Actual Chromium popup/native messaging, same-window countdown, navigation/deny/Stop, unknown-outcome tests; real close/discard/BFCache restore revoke pending permission | Durable sessions, other browser actions, actual tab replacement release acceptance |
| Pi project reads | Separate local-only read/list loop, internal owner root, exact one-shot path/line approval, bounded private worker, owner socket/turn cancellation | 15 real daemon tests, actual production CLI/local synthetic HTTP model/owner bridge/reader acceptance, 57 runtime tests | Actual local model/device acceptance, larger-project product feedback |
| File edits | Optional bounded exact-content previews and explicit JSON copy from already-approved snapshots. No apply | Production synthetic model/read/preview/final test and Chromium before/after UI. Files unchanged | Preparatory inert review workspace create/hash-recovery helpers tested; optional owner storage/UI has 8 daemon tests including lost-reply recovery; durable workspace browsing and safe commit/rollback/apply remain open |
| Preview validation | Optional owner-only Python/JSON/JavaScript syntax check under Linux namespaces/resource limits, no project code execution | 24 helper/protocol tests, 12 actual daemon tests including Stop/project-switch/EOF reaping, Chromium owner-only UI; never executes proposal code | Installed-device support and other language validators |
| Shell | Not enabled in Pi routes | No shell tool exposed | Safe execution boundary, exact command/cwd approval, output/deadline limits, child-group teardown, environment/secret policy, destructive-command review |
| OpenCode | Existing default and gateway preserved | 84 gateway tests earlier; bridge/assistant/streaming/session regressions | Later provider/integration convergence after Pi parity, not a plugin-only base |
| Desktop shell | Native GTK/WebKit shell, compatibility fallback, actual chat/layout/recovery acceptance | Native GTK screenshots and workflow fixtures | User's installed display/audio/device acceptance |
| Voice | Fixture-tested policy, streaming, cloud/Hex helpers and synthetic capture | Prior headless/policy/stream/helper suites | Live audio hardware and actual provider acceptance |
| Packaging | One consolidated patch against pinned upstream, manifest hashes, current zip, independent clean apply | Node 214 plus one skip, runtime57, Rust55, typechecks3, targeted daemon suites | Owner-approved publication and installation |

## Architecture choices

Keep each optional run fresh and scoped to an owner-selected project/document.
Never fall back to unrestricted runtime tools. A model request proposes work;
it cannot select an approval, root, recipient or provider. Failed/canceled
owner bridges abort the project run before another provider request. No hosted
provider is selected automatically. Local endpoints may themselves forward
information elsewhere; the owner must trust their provider configuration.

File edits should start with a bounded proposal against a known file digest,
not arbitrary shell or a model-authored patch command. Before enabling an apply
operation, implement exact reviewed content binding, component-wise no-symlink
opens, stale-content refusal and explicit handling of the race between approval,
cancel and commit. A successful write is not enough: the UI must show which
file changed and whether rollback is safe. Arbitrary shell is a separate
capability and cannot be smuggled through an edit helper.

## Gates

No spending, hosted Navi inference, merge, main-branch push or publication was
performed for this checkpoint. Publication/provider choices remain for the
owner. Tests use temporary synthetic projects and deterministic local providers.
A passing fixture is not certification of the user's hardware or every external
site. No 100% completion claim is made.

## Additional browser lifecycle acceptance

Real Chromium extension/native messaging/Station/local synthetic provider runs
now cover actual chrome.tabs.discard, tab close and BFCache restoration with
pageshow.persisted=true. Each begins with an observed focus request awaiting
approval. Release detaches the grant, rejects the old permission and prevents
focus. A fresh browser run without a new attach fails before any further local
provider request. An in-flight model final text can win a disconnect race; this
does not preserve the grant or permit an effect. Actual replacement remains
covered by unit fixtures only. Initial BFCache load-event wait timed out; using
commit navigation and checking persisted=true verified the actual restoration.

## Neon-led review, 13:05 Cairo

Gemini 4 Neon Extra High in AI Studio returned concrete review findings and
selected isolated candidate test execution as the next major slice. Confirmed
fixes: capture generation/epoch before selected-project validation and recheck
before spawn/return, clean lock-error handling, disable workspace UI before
awaiting request-ID lock, validate binding size before persistence, avoid signaling
a reaped validator PID, retain original failures through best-effort workspace
cleanup, classify unexpected JS runtime errors as unavailable, support CommonJS
hashbangs and filter Python SyntaxWarning from syntax parsing.

Validator24/workspace24, owner-validator12/owner-workspace8 and Chromium
metadata/reload acceptance pass after these changes. No clean-bill claim. The
Python warning case is a compatibility fix, not reproduced on this Python3.10
host. Candidate preparation is now implemented and tested, but actual isolated candidate
execution remains unavailable. Primary writes remain disabled.


## Neon-led candidate groundwork, 14:07 Cairo

Bounded internal snapshot preparation has22synthetic tests. Its public execution
APIs and CLI refuse unconditionally before project reads or storage creation.
A separate library-only one-shot snapshot approval prototype has26synthetic tests,
bound to exact fixed scope, opaque root identity and live epochs. Neither is
wired to a production route. Approval objects do not create authority. See
pi-project-candidates.md and pi-project-snapshot-approvals.md for boundaries.
A safe exclusive aggregate launcher, supported capped tmpfs, owner provisioning
approval and authoritative daemon/UI route are still missing. No cgroup settings
or host packages were changed. Whole-project contents are never automatically
sent to models. These are preparatory tests, not live execution acceptance.
