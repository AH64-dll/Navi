# Preparatory whole-project snapshot approval state machine

`workers/project_snapshot_approvals.py` is a library-only permission prototype. It reads no project files, creates no snapshots and runs no code. It is not connected to Rust, Station, model tools or the candidate snapshot builder. Its CLI reports that no authority was granted. Candidate execution remains unavailable.

The broker allows one pending request or approved unconsumed grant. Requests and grants expire, restart loses all state, and Stop/project-switch/disconnect invalidators clear it. Owner decisions are allow once, deny or cancel. Exact request ID, scope digest, root-identity digest, proposal digest, fixed descriptor and live project/generation/project-epoch/connection-epoch must match. Consumption burns a matching grant before checking drift. Locks allow at most one concurrent consumer. Clock high-water tracking refuses any observed backward movement.

The descriptor accepts only compiled snapshot defaults, including eligible extensions, file/directory/byte/depth/entry caps and local-only/no-model-disclosure/read-only invariants. Tighter apparent grants are refused too, because the snapshot builder is not parameterized to enforce them. Source files can contain secrets despite filename filtering.

A future trusted daemon must authenticate the owner connection, derive and componentwise revalidate root identity, bind decisions to current state, display the full snapshot scope and enforce invalidation across reads/spawn/return. Python object sentinels and channel objects prevent accidental API misuse, not malicious same-process code. A callback, JSON boolean or returned receipt cannot create standalone authority. The supplied root hash is not filesystem verification.

Opaque request/grant tokens use 192 random bits. Eight-attempt collision checks cover active/recent finalized state. The bounded 512-item history is diagnostic, not permanent uniqueness across all past tokens or restarts. Security outside that window depends on fresh cryptographic randomness and the future daemon's live context/connection binding.

Verification:

```sh
/usr/bin/python3 -I apps/navi/scripts/test_project_snapshot_approvals.py -v
```

26 synthetic tests cover context/root/scope drift, exact descriptor refusal, decision/consume replay, expiry, partial clock rollback, recent token collisions, invalidation, concurrency and library-only behavior. No production approval route or whole-project read capability is claimed.
