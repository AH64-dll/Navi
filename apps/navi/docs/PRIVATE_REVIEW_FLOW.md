# Private Review Flow

## Overview

Private review copies live under `<data_root>/proposal-reviews/` and store isolated review data for Navi without touching project source files. Maintainers and owners can list review metadata and delete individual review copies via owner-only, default-off Rust daemon RPC routes, Python workers, and the Station UI (`ui/station.js`).

Nothing in these features ever applies a review to project files. `workers/project_source_write.py` is a helper only and is not wired to any route or UI.

## Data layout

Private review copies live under:

`<data_root>/proposal-reviews/review-<32 hex>/`

Each review directory contains **exactly three files**:

- `before.txt`
- `proposed-after.txt`
- `review.json`

`review.json` carries:

- `review_id`
- `status`: `valid`
- `applied`: `false`
- `proposal_digest` (64 hex)
- `files` (list)

Nothing is ever applied to project files by these features.

## Routes and flags

The Rust daemon has owner-only, default-off RPC routes; each requires its env flag to be `1`. Replies never contain paths or file contents.

| Route | Env flag (must be `1`) | Behavior |
| --- | --- | --- |
| `pi_review_inventory` | `NAVI_PROJECT_REVIEW_INVENTORY` | Lists metadata only. |
| `pi_review_cleanup` | `NAVI_PROJECT_REVIEW_CLEANUP` | Deletes exactly one review copy. Requires `project_id`, `review_id`, `proposal_digest`, and `confirm == "delete private review <review_id>"`. |

| `pi_review_apply` | `NAVI_PROJECT_REVIEW_APPLY` | Applies one review to its project file. Requires `confirm == "apply private review <review_id>"`. |
| `pi_review_preview` | `NAVI_PROJECT_REVIEW_PREVIEW` | Read-only. Returns a bounded unified diff (before/after, at most 400 lines / 24000 chars) for one review. Requires `project_id`, `review_id`, `proposal_digest`. A stale reply (target changed) is flagged. |

| `pi_review_testrun` | `NAVI_PROJECT_REVIEW_TESTRUN` | Runs the project's unittest suite inside a bubblewrap sandbox on a throwaway private copy with the proposed edit applied. The real project is never written. Requires `project_id`, `review_id`, `proposal_digest`, and `confirm == "run tests on private copy <review_id>"`. Refuses with `sandbox_unavailable` if bwrap or its self-check is missing, and with `stale_target` if the project file changed. Copy skips symlinks, `.git`, `node_modules`, env and key-like files; caps 3000 files / 24 MiB. No network, read-only tree, one run at a time, 30 s limit, bounded output. |

Station requires a successful, non-stale preview for a row before its Apply control can be enabled. Diff text is shown with `textContent` only.

## Safety properties

- **Daemon boundary:** Routes are owner-only and default-off (each env flag must be `1`). Replies never contain paths or file contents.
- **Python worker controls:** Python workers use pinned directory fds, `O_NOFOLLOW`, owner-uid and mode checks, bounded directory listings, deadlines, and cooperative cancel.
- **`workers/project_review_inventory.py`:** Metadata listing only.
- **`workers/project_review_cleanup.py`:** Deletes exactly one review copy only after a digest match. Outcome-unknown reasons: `canceled`, `timed_out`, `directory_changed`.
- **`workers/project_source_write.py`:** Atomic single-file replace with a compare-and-swap on the old sha256. Helper only; **not** wired to any route or UI. Nothing applies reviews.

## UI behavior

Station UI (`ui/station.js`):

- **"Browse private review files"** button lists rows.
- Valid rows get a typed-phrase input (`delete private review <review_id>`) plus a **Delete** button.
- Only one delete runs at a time.
- After success, the list is re-fetched.
- Any error shows: `"Outcome unknown. Browse again before retrying."`
- Applying a review from the UI is **not implemented**.

## Tests

- Python worker and route tests:
  - `scripts/test_project_review_inventory.py`
  - `test_project_review_cleanup.py`
  - `*_owner.py` route tests
  - `test_project_source_write.py`
- UI tests:
  - `test_review_inventory_ui.mjs`
  - `test_review_cleanup_ui.mjs`
- Rust tests:
  - `cargo test`

## Not implemented

- Applying a review to project files
- Candidate-test execution (needs an isolation launcher the host lacks)
- Arbitrary-shell isolation
- Native WebKit/device acceptance
- Switching the default away from OpenCode
