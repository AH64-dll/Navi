# Rust snapshot context groundwork

`src/project_snapshot_context.rs` is metadata-only, not connected to daemon routes, snapshot reads, model tools, UI or execution. It adds no approval authority. Existing libc is reused; no new dependency or host setting.

On Linux, a held root opens each component with directory/nofollow/cloexec flags. Paths must be absolute and bounded, with no dot/dotdot, empty components, root-only path, symlinks or non-directory components. Ancestors must be root/user-owned, final root user-owned, and all must refuse group/world write. This is stricter than existing ProjectReader. A reopened path is compared against the held device/inode identity, detecting root replacement. A held FD keeps the original identity; metadata checks do not make an entire source snapshot atomic.

Raw identity is redacted in Debug. The digest hook supplies a domain-tagged device/inode payload to a caller's verified hash function, but does not implement SHA-256 or bridge to the Python approval module. No automatic path/hash disclosure exists.

A checked issuer creates nonzero monotonic connection/selection IDs with overflow refusal. IDs are unique only within one daemon-owned issuer lifetime, not authentication secrets. Restart must invalidate all requests. Per-connection liveness observers see explicit revoke and owner-handle drop without affecting unrelated connections. A future trusted daemon must own project mapping, issuer and authenticated connection binding.

Immutable contexts compare all fields and revalidate connection liveness plus held root identity. These are point-in-time checks. A future operation must keep the FD, hold suitable owner-scoped coordination, and recheck selection/generation/root/connection before read/spawn/return. No context grants permission by itself.

10 inline synthetic metadata tests pass. Full Rust suite now65passes. No production flow is changed besides declaring this unused module. Unsupported platforms fail closed. Native-user device/provider acceptance and actual candidate execution remain open.
