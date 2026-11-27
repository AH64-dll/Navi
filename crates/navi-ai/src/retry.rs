//! Provider retry policy, transient-failure classification, and backoff math.
//!
//! Ported from `packages/ai/src/utils/retry.ts` and
//! `packages/ai/src/utils/provider-retry.ts`. This module is pure: it never
//! sleeps, reads the clock, or draws randomness. Callers own the retry loop
//! and the actual waiting, while policy decisions stay deterministic and
//! reproducible from the supplied jitter seed.

use serde::{Deserialize, Serialize};

/// Default cap on a single retry delay (60 seconds).
pub const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// Default delay of the first retry (500 ms), matching the OpenAI and
/// Anthropic SDK retry timers.
pub const DEFAULT_BASE_DELAY_MS: u64 = 500;

/// Bounded retry policy for provider calls.
///
/// `max_retries` counts retries after the initial call. Delay for retry
/// `attempt` (1-indexed) is `base_delay_ms * 2^(attempt - 1)` before jitter,
/// capped by `max_delay_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Whether retries are enabled at all.
    pub enabled: bool,
    /// Maximum number of retries after the initial call (0 disables retries).
    pub max_retries: u32,
    /// Delay of the first retry in milliseconds.
    pub base_delay_ms: u64,
    /// Optional per-attempt delay cap; defaults to
    /// [`DEFAULT_MAX_RETRY_DELAY_MS`]. A value of zero means no waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delay_ms: Option<u64>,
}

impl RetryPolicy {
    /// Returns the effective cap applied to each computed delay.
    pub fn effective_max_delay_ms(&self) -> u64 {
        self.max_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: DEFAULT_BASE_DELAY_MS,
            max_delay_ms: None,
        }
    }
}

/// Outcome of evaluating one failed request against a [`RetryPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetryDecision {
    /// Another request should be made after `delay_ms` milliseconds.
    ///
    /// `attempt` is the 1-indexed number of the retry being scheduled.
    Retry { attempt: u32, delay_ms: u64 },
    /// Retries are disabled or the retry budget is exhausted.
    Exhausted,
    /// The failure is deterministic, so another attempt cannot help.
    NotRetryable,
}

impl RetryDecision {
    /// Reports whether another request should be made.
    pub fn should_retry(self) -> bool {
        matches!(self, Self::Retry { .. })
    }

    /// Returns the backoff delay when retrying, otherwise `None`.
    pub fn delay_ms(self) -> Option<u64> {
        match self {
            Self::Retry { delay_ms, .. } => Some(delay_ms),
            Self::Exhausted | Self::NotRetryable => None,
        }
    }
}

/// Evaluates a failed request and decides whether to retry it.
///
/// `attempt` is the 1-indexed retry number (the first retry is `1`). A status
/// takes precedence over message classification, except that lasting quota,
/// billing, and subscription limits are never retried. When no status is
/// available, the message alone decides. `retry_after_ms` and `jitter_seed`
/// are passed through to [`compute_backoff_ms`].
pub fn decide_retry(
    policy: &RetryPolicy,
    attempt: u32,
    status: Option<u16>,
    message: &str,
    retry_after_ms: Option<u64>,
    jitter_seed: u64,
) -> RetryDecision {
    if !policy.enabled || attempt == 0 || attempt > policy.max_retries {
        return RetryDecision::Exhausted;
    }

    let lower = message.to_ascii_lowercase();
    if is_non_retryable_error_lower(&lower) {
        return RetryDecision::NotRetryable;
    }

    let retryable = match status {
        Some(code) => is_retryable_status(code),
        None => is_retryable_error_lower(&lower),
    };
    if !retryable {
        return RetryDecision::NotRetryable;
    }

    RetryDecision::Retry {
        attempt,
        delay_ms: compute_backoff_ms(policy, attempt, retry_after_ms, jitter_seed),
    }
}

/// Reports whether an HTTP status denotes a transient provider failure.
///
/// Matches 408, 409, 429, 500, 502, 503, 504, and 529. Other 4xx and 5xx
/// statuses are treated as deterministic failures.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Reports whether an error message looks like a transient provider or
/// transport failure.
///
/// Lasting account limits (quota, billing, subscription exhaustion) are
/// checked first and always win. Matching is ASCII case-insensitive, and
/// numeric status codes must stand alone so request ids and byte counts do
/// not produce false positives.
pub fn is_retryable_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    !is_non_retryable_error_lower(&lower) && is_retryable_error_lower(&lower)
}

/// Reports whether an error message describes a lasting account or quota
/// limit, which must fail fast rather than be retried.
pub fn is_non_retryable_error(msg: &str) -> bool {
    is_non_retryable_error_lower(&msg.to_ascii_lowercase())
}

/// Lasting-limit phrases, checked before any retryable pattern.
fn is_non_retryable_error_lower(lower: &str) -> bool {
    const LIMIT_PHRASES: &[&str] = &[
        "go usage limit error",
        "gousagelimiterror",
        "free usage limit error",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
        "subscription_sharing_usage_limit_exceeded",
        "subscription_sharing_workplace_individual_usage_limit_exceeded",
    ];
    LIMIT_PHRASES.iter().any(|phrase| lower.contains(phrase)) || contains_json_code(lower, 1308)
}

/// Transient-failure phrases and the structural checks that need boundaries.
fn is_retryable_error_lower(lower: &str) -> bool {
    const TRANSIENT_PHRASES: &[&str] = &[
        // Provider load and capacity.
        "overloaded",
        "server_busy",
        "servers are currently busy",
        "currently experiencing high demand",
        "model is at capacity",
        "admission ceiling",
        "too many requests",
        "rate increased too quickly",
        "retry delay",
        // Server and gateway wording.
        "server error",
        "internal error",
        "provider returned error",
        "provider returned an error",
        "exceeded request buffer limit while retrying upstream",
        "unable to forward upstream",
        "upstream request failed",
        "upstream_stream_read_error",
        "stream was interrupted",
        "http2 stream failed",
        "http/2 stream failed",
        // Network and transport failures.
        "network error",
        "connection error",
        "connection refused",
        "connection lost",
        "connection closed",
        "other side closed",
        "fetch failed",
        "getaddrinfo",
        "connection reset by peer",
        "enotfound",
        "eai_again",
        "upstream connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed out",
        "timeout",
        "terminated",
        // WebSocket transports.
        "websocket closed",
        "websocket error",
        "websocket_connection_limit_reached",
        "websocket connection limit reached",
        // Streams that ended early or in an error.
        "ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        "provider finish_reason: error",
        "provider stopped with: error",
        "returned an empty response",
        "stopped before completing the response",
        "stream disconnected before completion",
        "cut short after the reply had begun",
        "ended the response before it was complete",
        "connection to the provider was reset",
        "reported a failure while generating the response",
        "pending stream has been canceled",
        // Explicit retry guidance and provider-specific codes.
        "you can retry your request",
        "try your request again",
        "please retry your request",
        "repeat the request later",
        "resourceexhausted",
        "subscription_sharing_usage_unavailable",
        "subscription_sharing_user_unavailable",
    ];
    TRANSIENT_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
        || contains_temporary_unavailable(lower)
        || contains_rate_limit(lower)
        || contains_status_code(lower)
        || contains_socket_code(lower)
        || contains_json_string_value(lower, "type", "api_error")
        || (lower.contains("request to ") && lower.contains(" failed, reason:"))
}

/// Matches "service unavailable", "provider temporarily unavailable", and
/// other transient unavailability wording, but not a lasting plain
/// "provider unavailable" routing error.
fn contains_temporary_unavailable(lower: &str) -> bool {
    lower.contains("temporarily unavailable")
        || (lower.contains("unavailable") && lower.contains("service"))
        || (lower.contains("unavailable")
            && lower.contains("provider")
            && lower.contains("temporarily"))
}

/// Matches `rate.?limit` (zero or one separator character).
fn contains_rate_limit(lower: &str) -> bool {
    for (start, _) in lower.match_indices("rate") {
        let after = &lower[start + "rate".len()..];
        if after.starts_with("limit") {
            return true;
        }
        if let Some(rest) = after.get(1..)
            && rest.starts_with("limit")
        {
            return true;
        }
    }
    false
}

/// Matches status codes only when they are not part of a longer word or
/// number, so `req_011CU503xyz` and `5242880 bytes` do not match.
fn contains_status_code(lower: &str) -> bool {
    const CODES: &[&str] = &[
        "408", "409", "429", "500", "502", "503", "504", "520", "524", "525", "529",
    ];
    // JavaScript's status pattern allows `_` as a boundary.
    CODES
        .iter()
        .any(|code| contains_bounded_token(lower, code, true))
}

/// Matches bare Node socket error codes, which must not be part of a longer
/// identifier such as `EXECONNRESET` or `ERR_EPIPE_ID`.
fn contains_socket_code(lower: &str) -> bool {
    const CODES: &[&str] = &[
        "econnreset",
        "econnaborted",
        "econnrefused",
        "etimedout",
        "epipe",
    ];
    CODES
        .iter()
        .any(|code| contains_bounded_token(lower, code, false))
}

fn contains_bounded_token(lower: &str, token: &str, underscore_is_boundary: bool) -> bool {
    let bytes = lower.as_bytes();
    let needle = token.as_bytes();
    if needle.is_empty() {
        return true;
    }
    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(start, window)| {
            window == needle
                && (start == 0 || !is_token_byte(bytes[start - 1], underscore_is_boundary))
                && (start + window.len() == bytes.len()
                    || !is_token_byte(bytes[start + window.len()], underscore_is_boundary))
        })
}

fn is_token_byte(byte: u8, underscore_is_boundary: bool) -> bool {
    byte.is_ascii_alphanumeric() || (byte == b'_' && !underscore_is_boundary)
}

/// Matches `"key"\s*:\s*"value"` with optional whitespace, as providers emit
/// error objects on a single line or pretty-printed across lines.
fn contains_json_string_value(lower: &str, key: &str, value: &str) -> bool {
    let quoted_key = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(found) = lower[search_from..].find(quoted_key.as_str()) {
        let after_key = search_from + found + quoted_key.len();
        let Some(rest) = lower.get(after_key..) else {
            break;
        };
        let Some(rest) = rest.trim_start().strip_prefix(':') else {
            search_from = after_key;
            continue;
        };
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix('"')
            && let Some(rest) = rest.strip_prefix(value)
            && rest.starts_with('"')
        {
            return true;
        }
        search_from = after_key;
    }
    false
}

/// Matches `"code"\s*:\s*"?1308` without treating 13080 as a match.
fn contains_json_code(lower: &str, code: u32) -> bool {
    let digits = code.to_string();
    let quoted_key = "\"code\"";
    let mut search_from = 0;
    while let Some(found) = lower[search_from..].find(quoted_key) {
        let after_key = search_from + found + quoted_key.len();
        let Some(rest) = lower.get(after_key..) else {
            break;
        };
        let Some(rest) = rest.trim_start().strip_prefix(':') else {
            search_from = after_key;
            continue;
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('"').unwrap_or(rest);
        if let Some(after) = rest.strip_prefix(digits.as_str())
            && !after.as_bytes().first().is_some_and(u8::is_ascii_digit)
        {
            return true;
        }
        search_from = after_key;
    }
    false
}

/// Parses a `Retry-After`-style header into milliseconds.
///
/// A plain number is seconds (`"2"` is 2000 ms), optionally fractional
/// (`"0.5"` is 500 ms). A trailing `ms` suffix, case-insensitive, marks the
/// value as milliseconds (`"1500ms"` is 1500 ms). Whitespace around the value
/// is ignored. Invalid, negative, non-finite, and HTTP-date values return
/// `None`, leaving the caller to fall back to computed backoff. Very large
/// finite values saturate at [`u64::MAX`].
pub fn parse_retry_after_ms(header: &str) -> Option<u64> {
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return None;
    }

    let is_millis =
        trimmed.len() >= 2 && trimmed.as_bytes()[trimmed.len() - 2..].eq_ignore_ascii_case(b"ms");
    let number = if is_millis {
        &trimmed[..trimmed.len() - 2]
    } else {
        trimmed
    };
    let value = number.trim().parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let millis = if is_millis { value } else { value * 1000.0 };
    if millis >= u64::MAX as f64 {
        return Some(u64::MAX);
    }
    Some(millis.round() as u64)
}

/// Computes the delay in milliseconds before retry `attempt` (1-indexed).
///
/// A server-provided delay takes priority and is returned unchanged apart
/// from the policy cap; jitter is only applied to locally computed backoff.
/// Otherwise the delay grows as `base_delay_ms * 2^(attempt - 1)`, saturating
/// on overflow, and is capped at `effective_max_delay_ms()`. Deterministic
/// jitter then removes 0% to 25% of the capped delay based on `jitter_seed`,
/// so the same seed always produces the same schedule.
pub fn compute_backoff_ms(
    policy: &RetryPolicy,
    attempt: u32,
    retry_after_ms: Option<u64>,
    jitter_seed: u64,
) -> u64 {
    let cap = policy.effective_max_delay_ms();
    if let Some(server_delay) = retry_after_ms {
        return server_delay.min(cap);
    }

    let exponent = attempt.saturating_sub(1).min(63);
    let exponential = policy.base_delay_ms.saturating_mul(1u64 << exponent);
    let capped = exponential.min(cap);
    capped.saturating_sub(jitter_reduction(capped, jitter_seed))
}

/// Removes between 0 and 250 permille of `delay_ms`, derived from the seed.
fn jitter_reduction(delay_ms: u64, seed: u64) -> u64 {
    const MAX_REDUCTION_PERMILLE: u64 = 250;
    let reduction = mix64(seed) % (MAX_REDUCTION_PERMILLE + 1);
    delay_ms.saturating_mul(reduction) / 1000
}

/// SplitMix64 finalizer: a small, deterministic, well-distributed mixer.
fn mix64(seed: u64) -> u64 {
    let mut value = seed ^ 0x9E37_79B9_7F4A_7C15;
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finds a seed with zero jitter so exact backoff values can be asserted.
    fn zero_jitter_seed() -> u64 {
        (0..4096)
            .find(|&seed| jitter_reduction(1_000_000, seed) == 0)
            .expect("a zero-jitter seed must exist in the search range")
    }

    #[test]
    fn retryable_statuses_match_the_provider_policy() {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(status), "{status} should be retryable");
        }
        for status in [
            0, 200, 301, 400, 401, 403, 404, 422, 501, 505, 520, 524, 525, 599,
        ] {
            assert!(
                !is_retryable_status(status),
                "{status} should not be retryable"
            );
        }
    }

    #[test]
    fn retryable_error_matches_transient_wording() {
        let messages = [
            "overloaded_error",
            "429 status code (no body)",
            "520 status code (no body)",
            "Rate limit exceeded",
            "provider finish_reason: error",
            r#"{"type":"error","error":{"type":"api_error","message":"Unable to complete this request right now."}}"#,
            "  { \"type\" : \"api_error\" }  ",
            "read ECONNRESET",
            "connect ETIMEDOUT 140.82.113.3:443",
            "The socket connection was closed unexpectedly",
            "request to https://oauth2.googleapis.com/token failed, reason:",
            "The provider is temporarily unavailable. Retry shortly.",
            "Codex error: websocket_connection_limit_reached",
            "waited 73.4s in queue, reaching this model's admission ceiling of 60s",
            "You can retry your request",
            "ResourceExhausted: Worker local total request limit reached",
            "subscription_sharing_usage_unavailable: Usage cannot be checked.",
        ];
        for message in messages {
            assert!(is_retryable_error(message), "should retry: {message}");
        }
    }

    #[test]
    fn lasting_limits_and_embedded_codes_are_not_retryable() {
        let messages = [
            "429 quota exceeded",
            "insufficient_quota",
            "Monthly quota exceeded",
            "FreeUsageLimitError",
            "Your account is out of budget",
            "billing hard limit reached",
            "monthly usage limit reached; enable available balance",
            "subscription_sharing_usage_limit_exceeded",
            r#""code": 1308"#,
            r#""code":"1308""#,
            "Provider unavailable for this model",
            "The provider is unavailable in your region",
            "The operation was aborted",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#,
            "Provider finish_reason: content_filter",
            "Invalid value EXECONNRESET",
            "Unknown field ETIMEDOUTX",
            "Invalid key ERR_EPIPE_ID",
            "400 image exceeds 5 MB maximum: 5316784 bytes > 5242880 bytes",
            r#"{"request_id":"req_011CU503xyz"}"#,
        ];
        for message in messages {
            assert!(!is_retryable_error(message), "must not retry: {message}");
        }
        for limit_message in [
            "429 quota exceeded",
            "FreeUsageLimitError",
            "subscription_sharing_usage_limit_exceeded",
        ] {
            assert!(is_non_retryable_error(limit_message));
        }
        assert!(!is_non_retryable_error("overloaded"));
    }

    #[test]
    fn retry_after_header_parses_seconds_and_milliseconds() {
        assert_eq!(parse_retry_after_ms("2"), Some(2000));
        assert_eq!(parse_retry_after_ms(" 0.5 "), Some(500));
        assert_eq!(parse_retry_after_ms("1500ms"), Some(1500));
        assert_eq!(parse_retry_after_ms("1500 MS"), Some(1500));
        assert_eq!(parse_retry_after_ms("0"), Some(0));
        assert_eq!(parse_retry_after_ms("1e3"), Some(1_000_000));
        for invalid in [
            "",
            "   ",
            "soon",
            "Wed, 21 Oct 2015 07:28:00 GMT",
            "-1",
            "NaN",
        ] {
            assert_eq!(parse_retry_after_ms(invalid), None, "invalid: {invalid}");
        }
        assert_eq!(parse_retry_after_ms("1e308"), Some(u64::MAX));
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let policy = RetryPolicy {
            base_delay_ms: 100,
            max_delay_ms: None,
            ..RetryPolicy::default()
        };
        let seed = zero_jitter_seed();
        for (attempt, expected) in [(1, 100), (2, 200), (3, 400), (4, 800), (5, 1600)] {
            assert_eq!(compute_backoff_ms(&policy, attempt, None, seed), expected);
        }

        let capped = RetryPolicy {
            max_delay_ms: Some(500),
            ..policy
        };
        assert_eq!(compute_backoff_ms(&capped, 1, None, seed), 100);
        assert_eq!(compute_backoff_ms(&capped, 3, None, seed), 400);
        assert_eq!(compute_backoff_ms(&capped, 4, None, seed), 500);
        assert_eq!(compute_backoff_ms(&capped, 30, None, seed), 500);
    }

    #[test]
    fn backoff_handles_zero_overflow_and_server_delay() {
        let seed = zero_jitter_seed();
        let base = RetryPolicy {
            base_delay_ms: 0,
            ..RetryPolicy::default()
        };
        assert_eq!(compute_backoff_ms(&base, 1, None, seed), 0);
        assert_eq!(compute_backoff_ms(&base, 100, None, seed), 0);

        let disabled_delay = RetryPolicy {
            max_delay_ms: Some(0),
            ..RetryPolicy::default()
        };
        assert_eq!(compute_backoff_ms(&disabled_delay, 1, None, seed), 0);
        assert_eq!(compute_backoff_ms(&disabled_delay, 1, Some(9000), seed), 0);

        let huge = RetryPolicy {
            base_delay_ms: u64::MAX,
            max_delay_ms: Some(1234),
            ..RetryPolicy::default()
        };
        assert_eq!(compute_backoff_ms(&huge, 1, None, seed), 1234);
        assert_eq!(compute_backoff_ms(&huge, 64, None, seed), 1234);

        let normal = RetryPolicy::default();
        assert_eq!(compute_backoff_ms(&normal, 1, Some(2500), seed), 2500);
        assert_eq!(
            compute_backoff_ms(&normal, 1, Some(90_000), seed),
            DEFAULT_MAX_RETRY_DELAY_MS
        );
        assert!(compute_backoff_ms(&normal, 0, None, seed) <= 500);
    }

    #[test]
    fn jitter_is_deterministic_and_stays_within_bounds() {
        let policy = RetryPolicy {
            base_delay_ms: 1000,
            max_delay_ms: None,
            ..RetryPolicy::default()
        };
        let mut delays = std::collections::BTreeSet::new();
        for seed in 0..64 {
            let first = compute_backoff_ms(&policy, 1, None, seed);
            assert_eq!(first, compute_backoff_ms(&policy, 1, None, seed));
            assert!(
                (750..=1000).contains(&first),
                "jitter out of range: {first}"
            );
            delays.insert(first);
        }
        assert!(delays.len() > 1, "jitter must vary with the seed");

        let capped = RetryPolicy {
            max_delay_ms: Some(100),
            ..policy
        };
        for seed in 0..64 {
            let delay = compute_backoff_ms(&capped, 10, None, seed);
            assert!((75..=100).contains(&delay), "capped jitter out of range");
        }
    }

    #[test]
    fn decide_retry_respects_budget_and_classification() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 2,
            base_delay_ms: 1000,
            max_delay_ms: Some(800),
        };
        assert_eq!(
            decide_retry(&policy, 1, Some(503), "busy", None, zero_jitter_seed()),
            RetryDecision::Retry {
                attempt: 1,
                delay_ms: 800
            }
        );
        assert_eq!(
            decide_retry(&policy, 2, None, "terminated", Some(250), 0),
            RetryDecision::Retry {
                attempt: 2,
                delay_ms: 250
            }
        );
        assert_eq!(
            decide_retry(&policy, 3, Some(429), "busy", None, 0),
            RetryDecision::Exhausted
        );
        assert_eq!(
            decide_retry(&policy, 0, Some(429), "busy", None, 0),
            RetryDecision::Exhausted
        );
        assert_eq!(
            decide_retry(
                &RetryPolicy {
                    enabled: false,
                    ..policy
                },
                1,
                Some(503),
                "busy",
                None,
                0
            ),
            RetryDecision::Exhausted
        );
        assert_eq!(
            decide_retry(&policy, 1, Some(400), "bad request", None, 0),
            RetryDecision::NotRetryable
        );
        assert_eq!(
            decide_retry(&policy, 1, Some(429), "insufficient_quota", None, 0),
            RetryDecision::NotRetryable
        );
        assert_eq!(
            decide_retry(&policy, 1, None, "invalid api key", None, 0),
            RetryDecision::NotRetryable
        );

        let decision = decide_retry(
            &policy,
            1,
            Some(529),
            "overloaded",
            None,
            zero_jitter_seed(),
        );
        assert!(decision.should_retry());
        assert_eq!(decision.delay_ms(), Some(800));
        assert!(!RetryDecision::Exhausted.should_retry());
        assert_eq!(RetryDecision::NotRetryable.delay_ms(), None);
    }

    #[test]
    fn policy_and_decision_roundtrip_through_serde() {
        let policy = RetryPolicy {
            enabled: true,
            max_retries: 5,
            base_delay_ms: 2000,
            max_delay_ms: Some(15_000),
        };
        let encoded = serde_json::to_string(&policy).expect("serialize policy");
        let decoded: RetryPolicy = serde_json::from_str(&encoded).expect("deserialize policy");
        assert_eq!(decoded, policy);

        let minimal: RetryPolicy =
            serde_json::from_str(r#"{"enabled":true,"max_retries":3,"base_delay_ms":500}"#)
                .expect("deserialize minimal policy");
        assert_eq!(minimal.max_delay_ms, None);
        assert_eq!(minimal.effective_max_delay_ms(), DEFAULT_MAX_RETRY_DELAY_MS);

        for decision in [
            RetryDecision::Retry {
                attempt: 2,
                delay_ms: 1234,
            },
            RetryDecision::Exhausted,
            RetryDecision::NotRetryable,
        ] {
            let encoded = serde_json::to_string(&decision).expect("serialize decision");
            let decoded: RetryDecision =
                serde_json::from_str(&encoded).expect("deserialize decision");
            assert_eq!(decoded, decision);
        }
    }
}
