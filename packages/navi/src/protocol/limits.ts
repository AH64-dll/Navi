/**
 * Fixed protocol limits. Both peers import these constants; nothing is negotiated at runtime.
 * Browser-safe: no Node APIs, no side effects.
 */

/** NAVI shared protocol version. Bumped only for a breaking frame or payload change. */
export const NAVI_PROTOCOL_VERSION = 1 as const;

/** Capabilities a user grant may enable. `observe` is the default; the rest are opt-in. */
export const CAPABILITIES = ["observe", "focus", "scroll"] as const;

/** Largest serialized envelope accepted in either direction, in UTF-8 bytes. */
export const MAX_ENVELOPE_BYTES = 64 * 1024;

/** Largest serialized observation payload, in UTF-8 bytes. Leaves envelope headroom. */
export const MAX_OBSERVATION_BYTES = 16 * 1024;

/** Largest number of DOM nodes returned in one observation (flat list). */
export const MAX_NODES = 100;

/** Largest number of characters kept per node `name` and `text`. */
export const MAX_TEXT_CHARS = 200;

/** Collector element visit budget before the content script reports a truncated capture. */
export const MAX_VISITED_ELEMENTS = 1500;

/** Browser operations in flight per connection. One keeps grants and refs unambiguous. */
export const MAX_INFLIGHT_BROWSER_OPS = 1;

/** Outbound queue bounds per direction. */
export const MAX_QUEUE_FRAMES = 16;
export const MAX_QUEUE_BYTES = 2 * MAX_ENVELOPE_BYTES;

/** Largest single scroll delta accepted in a `scroll` request, in CSS pixels. */
export const MAX_SCROLL_DELTA = 1000;

/** Bounded identifier and message lengths. */
export const MAX_ID_CHARS = 64;
export const MAX_VERSION_CHARS = 32;
export const MAX_URL_CHARS = 512;
export const MAX_TITLE_CHARS = 200;
export const MAX_ERROR_MESSAGE_CHARS = 200;

/** Timing limits in milliseconds. */
export const HANDSHAKE_TIMEOUT_MS = 5_000;
export const OBSERVE_TIMEOUT_MS = 4_000;
export const INTERACTION_TIMEOUT_MS = 2_000;
export const DRAIN_GRACE_MS = 1_000;
export const IDLE_CLOSE_MS = 60_000;

/** Highest accepted sequence number. Identical values are rejected: sequence is strictly increasing. */
export const MAX_SEQUENCE = Number.MAX_SAFE_INTEGER;
