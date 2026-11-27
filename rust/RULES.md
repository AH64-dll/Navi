# Port rules

Behavior the port must keep exactly, learned from corrections. Every agent prompt rust-anything writes includes these rules. Edit them by hand, or add one with `rust-anything rules --add`.

- Zero unsafe blocks in src/port.rs.
- Zero unwrap() or expect() on untrusted input; return proper Err/Result values.
- Preserve exact error type names and error messages from the original implementation.
- For TypeScript ports, preserve UTF-16 string indexing and slice offsets where JS semantics apply.
- For filesystem/path operations, preserve strict O_NOFOLLOW, canonical containment, and owner-only permission checks.
- Never hardcode recorded test outputs; implement the real algorithm so fuzzing on unseen inputs passes 100%.
- Code must pass cargo clippy --all-targets -- -D warnings and cargo fmt --check with zero warnings.
