# Preparatory candidate snapshots

Status: candidate code execution is NOT IMPLEMENTED. No model, UI, Rust or daemon route invokes this helper. OpenCode stays the default. Primary project edits remain disabled.

`workers/project_candidate_test.py` contains an internal snapshot builder and synthetic bounded-output utilities. Every public candidate execution entrypoint, including the CLI, unconditionally refuses before project reads or storage creation. No environment flag bypasses this refusal. The boolean approval parameter is a defensive precondition for internal tests, not owner authority.

## Snapshot boundaries

Synthetic tests exercise copying eligible UTF-8 `.py`, `.json`, `.toml` and `.txt` regular files into owner-private storage, then replacing one candidate file only when its live bytes match the preview's before hash. Bounds: 256 files, 256 created directories, 64 KiB per file, 4 MiB total, 4096 inspected entries and 16 directory levels. Listings are streamed into bounded arrays. Hidden paths are skipped. Symlinks, hardlinks, cross-filesystem entries, FIFOs and other nonregular entries are refused. Eligible files containing NUL or invalid UTF-8 are refused.

Candidate storage and project paths must not overlap. Files are created exclusively with mode 0600 under 0700 directories. Failed or canceled preparation cleans only directories created by that invocation. Identifier collisions preserve existing contents. Cleanup is bounded and never follows symlinks. Abrupt worker death can leave private files requiring later owner-managed cleanup.

This is not an atomic whole-project snapshot. Checks detect changes during individual reads, not all edits across the tree. Hidden-path and extension filtering do not remove secrets embedded in eligible files. No snapshot or test output may be sent automatically to a model.

## Execution prerequisites

Actual execution requires a separately reviewed exclusive aggregate process/memory launcher, explicit owner provisioning approval, supported capped tmpfs and fresh daemon-bound whole-project snapshot approval tied to the active project, generation, project epoch, runner and snapshot extent. A shared cgroup leaf is not accepted because killing it can affect unrelated processes. No host cgroup settings are created or changed by this helper. Current verification host has bubblewrap 0.6.1, without the proposed capped-tmpfs option. No package installation or weaker fallback was performed.

The internal bounded process utility is for trusted synthetic fixtures only. It is not a project sandbox and has no aggregate process or memory isolation. Process-group teardown alone cannot contain untrusted descendants that start another session. A zero exit code is not proof that substantive tests ran or passed.

## Verification

Run from the repository root:

```sh
/usr/bin/python3 -I apps/navi/scripts/test_project_candidate_test.py -v
```

The current suite has 22 synthetic tests with no skips, including unconditional execution refusal before I/O, snapshot limits, text/file safety, stale hashes, collision preservation, cancellation cleanup and bounded synthetic output/timeouts. These results do not claim live project execution acceptance.
