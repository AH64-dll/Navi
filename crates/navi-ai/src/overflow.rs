//! Context-window and token-limit overflow detection for provider errors.
//!
//! Providers report an exhausted context window in many different ways: some
//! name the token limit, some name the context window, some only report an
//! oversized HTTP request. This module recognizes the common Anthropic,
//! OpenAI, Google Gemini, OpenRouter, vLLM, Ollama, and generic phrasings with
//! case-insensitive string matching, so no regular-expression crate is needed.
//!
//! Error text that clearly describes throttling or rate limiting is excluded,
//! because those messages can contain phrases such as "too many tokens"
//! without being a context overflow.

use serde::{Deserialize, Serialize};

/// Category of a detected context or request-size overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowKind {
    /// The input no longer fits the model's context window or token budget.
    ContextWindow,
    /// The provider rejected the whole request as too large (typically HTTP 413).
    RequestTooLarge,
    /// The provider explicitly reported the prompt as too long.
    PromptTooLong,
    /// A generic token limit was exceeded.
    TokenLimit,
}

/// Returns `true` when `message` reports a context-window or token-limit overflow.
///
/// Matching is case-insensitive and tolerant of extra whitespace. Messages that
/// describe rate limiting or throttling are excluded even when they contain an
/// overflow phrase.
pub fn is_context_overflow_error(message: &str) -> bool {
    let normalized = normalize(message);
    detect(&normalized).is_some()
}

/// Classifies an overflow from an optional HTTP status code and an error message.
///
/// HTTP 413 is a request-size overflow regardless of the body text. A message
/// describing throttling or rate limiting is never classified as an overflow,
/// even when an HTTP 413 status is supplied.
pub fn classify_overflow(status: Option<u16>, message: &str) -> Option<OverflowKind> {
    let normalized = normalize(message);
    if is_non_overflow(&normalized) {
        return None;
    }
    if status == Some(413) {
        return Some(OverflowKind::RequestTooLarge);
    }
    detect(&normalized)
}

/// Returns the first overflow category matched by a normalized message.
fn detect(normalized: &str) -> Option<OverflowKind> {
    if is_non_overflow(normalized) {
        return None;
    }

    // Request-size limits are the most specific signal: Anthropic's error code,
    // SDK 413 prefixes, Cerebras' bodiless 400/413 errors, and the standard
    // "request entity too large" wording.
    if contains_word(normalized, "request_too_large")
        || starts_with_413(normalized)
        || starts_with_api_error_413(normalized)
        || is_bodiless_size_error(normalized)
        || normalized.contains("request exceeds the maximum size")
        || normalized.contains("request entity too large")
        || normalized.contains("payload too large")
    {
        return Some(OverflowKind::RequestTooLarge);
    }

    // Phrases that explicitly blame the prompt or its length.
    if normalized.contains("prompt too long")
        || normalized.contains("prompt is too long")
        || normalized.contains("prompt exceeds max length")
        || normalized.contains("input is too long for requested model")
        || has_digits_after(normalized, "maximum prompt length is")
        || normalized.contains("reduce the length of the messages")
        || (normalized.contains("prompt has") && normalized.contains("configured context size is"))
    {
        return Some(OverflowKind::PromptTooLong);
    }

    // Phrases that name a context window, a maximum context length, or a token
    // count that is compared against a maximum.
    if normalized.contains("exceeds the context window")
        || normalized.contains("exceeds context window")
        || normalized.contains("context window exceeded")
        || normalized.contains("context_window_exceeded")
        || normalized.contains("context length exceeded")
        || normalized.contains("context_length_exceeded")
        || normalized.contains("exceeded max context length")
        || normalized.contains("exceeded context length")
        || (normalized.contains("exceeds") && normalized.contains("maximum context length"))
        || has_digits_after(normalized, "maximum context length is")
        || has_digits_after(normalized, "max context length is")
        || contains_before(normalized, "input token count", "exceeds the maximum")
        || contains_before(normalized, "input token count", "exceeds maximum")
        || normalized.contains("exceeds the maximum allowed input length of")
        || normalized.contains("is longer than the model's context length")
        || normalized.contains("is longer than model context length")
        || normalized.contains("exceeds the available context size")
        || normalized.contains("greater than the context length")
        || normalized.contains("context window exceeds limit")
        || (normalized.contains("too large for model with")
            && normalized.contains("maximum context length"))
        || normalized.contains("model_context_window_exceeded")
        || normalized.contains("range of input length should be")
    {
        return Some(OverflowKind::ContextWindow);
    }

    // Generic token-budget wording is checked last so a more specific category
    // wins when a message contains several signals.
    if normalized.contains("too many tokens")
        || normalized.contains("token limit exceeded")
        || normalized.contains("exceeded model token limit")
        || normalized.contains("exceeded token limit")
        || has_digits_after(normalized, "exceeds the limit of")
    {
        return Some(OverflowKind::TokenLimit);
    }

    None
}

/// Returns `true` for errors whose wording rules out an overflow.
fn is_non_overflow(normalized: &str) -> bool {
    normalized.starts_with("throttling error:")
        || normalized.starts_with("throttlingexception:")
        || normalized.starts_with("service unavailable:")
        || normalized.contains("rate limit")
        || normalized.contains("too many requests")
}

/// Lowercases `message` and collapses whitespace runs to single spaces.
///
/// Normalizing once lets every pattern be a plain substring check instead of a
/// regex with optional whitespace, and keeps leading/trailing whitespace from
/// breaking the anchored 413 prefixes.
fn normalize(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut pending_space = false;
    for ch in message.chars() {
        if ch.is_ascii_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.extend(ch.to_lowercase());
    }
    out
}

/// Returns `true` for an ASCII word byte, used for identifier boundaries.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Finds `word` only when it is not part of a longer identifier.
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.match_indices(word).any(|(idx, _)| {
        let before_ok = idx == 0 || !is_word_byte(haystack.as_bytes()[idx - 1]);
        let end = idx + word.len();
        let after_ok = end == haystack.len() || !is_word_byte(haystack.as_bytes()[end]);
        before_ok && after_ok
    })
}

/// Finds `second` somewhere after `first`, matching the regex `first.*second`.
fn contains_before(haystack: &str, first: &str, second: &str) -> bool {
    let Some(idx) = haystack.find(first) else {
        return false;
    };
    haystack[idx + first.len()..].contains(second)
}

/// Finds `prefix` followed by optional whitespace and at least one decimal digit.
fn has_digits_after(haystack: &str, prefix: &str) -> bool {
    haystack.match_indices(prefix).any(|(idx, _)| {
        let rest = &haystack[idx + prefix.len()..];
        let rest = if let Some(rest) = rest.strip_prefix(' ') {
            rest
        } else {
            rest
        };
        rest.as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_digit())
    })
}

/// Matches a bare `413` at the start of a raw SDK error, like `413: ...`.
fn starts_with_413(normalized: &str) -> bool {
    let Some(rest) = normalized.strip_prefix("413") else {
        return false;
    };
    matches!(rest.as_bytes().first(), None | Some(&b' ') | Some(&b':'))
}

/// Matches a formatProviderError prefix such as `github-copilot API error (413):`.
fn starts_with_api_error_413(normalized: &str) -> bool {
    let Some(idx) = normalized.find("api error (413):") else {
        return false;
    };
    let prefix = normalized[..idx].trim_end();
    !prefix.is_empty()
        && prefix.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b' ' || byte == b'-'
        })
}

/// Matches Cerebras-style bodiless size errors: `400 status code (no body)`.
fn is_bodiless_size_error(normalized: &str) -> bool {
    let Some(rest) = normalized
        .strip_prefix("400 ")
        .or_else(|| normalized.strip_prefix("413 "))
    else {
        return false;
    };
    let rest = if let Some(rest) = rest.strip_prefix("status code ") {
        rest
    } else {
        rest
    };
    rest == "(no body)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_anthropic_prompt_and_request_size_errors() {
        let cases = [
            (
                "prompt is too long: 213462 tokens > 200000 maximum",
                OverflowKind::PromptTooLong,
            ),
            ("PROMPT TOO LONG", OverflowKind::PromptTooLong),
            (
                "413 {\"error\":{\"type\":\"request_too_large\",\"message\":\"Request exceeds the maximum size\"}}",
                OverflowKind::RequestTooLarge,
            ),
            (
                "github-copilot API error (413): failed to parse request",
                OverflowKind::RequestTooLarge,
            ),
        ];
        for (message, expected) in cases {
            assert!(is_context_overflow_error(message), "{message}");
            assert_eq!(
                classify_overflow(None, message),
                Some(expected),
                "{message}"
            );
        }
    }

    #[test]
    fn detects_openai_and_litellm_context_phrases() {
        let cases = [
            (
                "Your input exceeds the context window of this model",
                OverflowKind::ContextWindow,
            ),
            (
                "Requested token count exceeds the model's maximum context length of 131072 tokens",
                OverflowKind::ContextWindow,
            ),
            (
                "Input length (265330) exceeds model's maximum context length (262144).",
                OverflowKind::ContextWindow,
            ),
            (
                "litellm.ContextWindowExceededError: Context Window exceeded for given call.",
                OverflowKind::ContextWindow,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                classify_overflow(None, message),
                Some(expected),
                "{message}"
            );
        }
    }

    #[test]
    fn detects_google_input_token_count_overflow() {
        let google = "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)";
        assert!(is_context_overflow_error(google));
        assert_eq!(
            classify_overflow(None, google),
            Some(OverflowKind::ContextWindow)
        );
        assert_eq!(
            classify_overflow(None, "input token count 500 EXCEEDS MAXIMUM tokens"),
            Some(OverflowKind::ContextWindow)
        );
    }

    #[test]
    fn detects_openrouter_vllm_ollama_and_other_providers() {
        let cases = [
            (
                "This endpoint's maximum context length is 131072 tokens. However, you requested about 200000 tokens",
                OverflowKind::ContextWindow,
            ),
            (
                "Input length 265330 exceeds the maximum allowed input length of 262144 tokens.",
                OverflowKind::ContextWindow,
            ),
            (
                "The input (265330 tokens) is longer than the model's context length (262144 tokens).",
                OverflowKind::ContextWindow,
            ),
            (
                "This model's maximum context length is 8192 tokens. However, you requested 10000 tokens",
                OverflowKind::ContextWindow,
            ),
            (
                "the request exceeds the available context size, try increasing it",
                OverflowKind::ContextWindow,
            ),
            (
                "tokens to keep from the initial prompt is greater than the context length",
                OverflowKind::ContextWindow,
            ),
            (
                "Prompt has 300000 tokens, but the configured context size is 262144 tokens",
                OverflowKind::PromptTooLong,
            ),
            (
                "invalid params, context window exceeds limit",
                OverflowKind::ContextWindow,
            ),
            (
                "Prompt contains 300000 tokens ... too large for model with 262144 maximum context length",
                OverflowKind::ContextWindow,
            ),
            (
                "Range of input length should be [1, 131072]",
                OverflowKind::ContextWindow,
            ),
            (
                "prompt too long; exceeded max context length by 1234 tokens",
                OverflowKind::PromptTooLong,
            ),
            (
                "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
                OverflowKind::PromptTooLong,
            ),
            (
                "Please reduce the length of the messages or completion",
                OverflowKind::PromptTooLong,
            ),
            (
                "{\"code\":\"1261\",\"message\":\"Prompt exceeds max length\"}",
                OverflowKind::PromptTooLong,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                classify_overflow(None, message),
                Some(expected),
                "{message}"
            );
        }
    }

    #[test]
    fn detects_generic_context_and_token_limit_phrases() {
        let cases = [
            ("context length exceeded", OverflowKind::ContextWindow),
            ("CONTEXT_LENGTH_EXCEEDED", OverflowKind::ContextWindow),
            ("model_context_window_exceeded", OverflowKind::ContextWindow),
            ("too many tokens in the request", OverflowKind::TokenLimit),
            ("token limit exceeded", OverflowKind::TokenLimit),
            (
                "Your request exceeded model token limit: 300000 (requested: 400000)",
                OverflowKind::TokenLimit,
            ),
            (
                "prompt token count of 300000 exceeds the limit of 262144",
                OverflowKind::TokenLimit,
            ),
            ("413 status code (no body)", OverflowKind::RequestTooLarge),
            ("400 status code (no body)", OverflowKind::RequestTooLarge),
            ("Request entity too large", OverflowKind::RequestTooLarge),
        ];
        for (message, expected) in cases {
            assert!(is_context_overflow_error(message), "{message}");
            assert_eq!(
                classify_overflow(None, message),
                Some(expected),
                "{message}"
            );
        }
    }

    #[test]
    fn excludes_rate_limit_and_throttling_errors() {
        let messages = [
            "Throttling error: slow down",
            "Service unavailable: try later",
            "rate limit exceeded; too many tokens",
            "429 Too many requests: too many tokens",
            "ThrottlingException: Too many tokens, please wait before trying again.",
        ];
        for message in messages {
            assert!(!is_context_overflow_error(message), "{message}");
            assert_eq!(classify_overflow(None, message), None, "{message}");
            assert_eq!(classify_overflow(Some(413), message), None, "{message}");
        }
    }

    #[test]
    fn status_413_overrides_uninformative_body() {
        assert_eq!(
            classify_overflow(Some(413), "opaque transport failure"),
            Some(OverflowKind::RequestTooLarge)
        );
        assert_eq!(
            classify_overflow(Some(500), "prompt is too long"),
            Some(OverflowKind::PromptTooLong)
        );
        assert_eq!(classify_overflow(Some(500), "null"), None);
        assert_eq!(
            classify_overflow(None, "prompt is too long"),
            Some(OverflowKind::PromptTooLong)
        );
    }

    #[test]
    fn rejects_empty_unrelated_and_identifier_like_messages() {
        for message in [
            "",
            "   \n\t  ",
            "the model answered normally",
            "my_request_too_large_flag was disabled",
            "request_too_large_v2",
            "maximum prompt length is unknown",
        ] {
            assert!(!is_context_overflow_error(message), "{message}");
            assert_eq!(classify_overflow(Some(500), message), None, "{message}");
        }
        assert_eq!(
            classify_overflow(None, "error: REQUEST_TOO_LARGE"),
            Some(OverflowKind::RequestTooLarge)
        );
    }

    #[test]
    fn handles_mixed_case_unicode_and_extra_whitespace() {
        let spaced = "  PROMPT   IS\nTOO LONG : 300000 tokens > 200000  ";
        assert_eq!(
            classify_overflow(None, spaced),
            Some(OverflowKind::PromptTooLong)
        );
        let unicode = "Requête trop longue — context window exceeded for modèle";
        assert!(is_context_overflow_error(unicode));
        assert_eq!(
            classify_overflow(None, unicode),
            Some(OverflowKind::ContextWindow)
        );
        assert_eq!(normalize("  A\t B\nC  "), "a b c");
    }

    #[test]
    fn detects_bedrock_and_generic_fallback_variants() {
        let cases = [
            (
                "Input is too long for requested model",
                OverflowKind::PromptTooLong,
            ),
            ("exceeded context length", OverflowKind::ContextWindow),
            ("context_window_exceeded", OverflowKind::ContextWindow),
            ("413: transport failure", OverflowKind::RequestTooLarge),
            (
                "custom-gateway API error (413): failed to parse request",
                OverflowKind::RequestTooLarge,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                classify_overflow(None, message),
                Some(expected),
                "{message}"
            );
            assert!(is_context_overflow_error(message), "{message}");
        }
    }

    #[test]
    fn overflow_kind_roundtrips_through_serde() {
        let kinds = [
            OverflowKind::ContextWindow,
            OverflowKind::RequestTooLarge,
            OverflowKind::PromptTooLong,
            OverflowKind::TokenLimit,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).expect("serialize");
            let decoded: OverflowKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, kind);
        }
        assert_eq!(
            serde_json::to_string(&OverflowKind::RequestTooLarge).expect("serialize"),
            "\"request_too_large\""
        );
    }
}
