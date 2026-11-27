//! Secret protection helpers for the sandbox.
//!
//! [`SecretString`] keeps secret material in a plain byte buffer so the bytes
//! can be overwritten on drop using safe Rust, and never prints the plaintext
//! through [`std::fmt::Debug`] or [`std::fmt::Display`]. [`redact_secrets`]
//! and [`contains_secret`] scan untrusted text for common credential formats
//! without requiring a regex engine.

use navi_types::{NaviError, NaviResult};
use std::fmt;
use std::sync::atomic::{Ordering, compiler_fence};

/// Marker written in place of every detected credential.
pub const REDACTED: &str = "[REDACTED]";

/// Token-style credential prefixes and the minimum number of token bytes that
/// must follow each prefix before it is treated as a secret.
const TOKEN_PREFIXES: &[(&[u8], usize)] = &[
    (b"sk-ant-", 1), // Anthropic
    (b"oc_sk_", 1),  // OpenCode
    (b"sk-", 1),     // OpenAI
    (b"ghp_", 1),    // GitHub personal access token
    (b"gho_", 1),    // GitHub OAuth token
    (b"AKIA", 1),    // AWS long-term access key id
];

const PEM_BEGIN: &[u8] = b"-----BEGIN ";
const PEM_END: &[u8] = b"-----END ";
const PRIVATE_KEY_TAIL: &[u8] = b"PRIVATE KEY-----";
const BEARER: &[u8] = b"bearer";

/// A string that redacts itself in formatted output and wipes its bytes on drop.
pub struct SecretString {
    bytes: Vec<u8>,
}

impl SecretString {
    /// Wraps a secret string.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            bytes: secret.into().into_bytes(),
        }
    }

    /// Wraps secret bytes after checking that they are valid UTF-8.
    pub fn from_bytes(bytes: Vec<u8>) -> NaviResult<Self> {
        std::str::from_utf8(&bytes)
            .map_err(|err| NaviError::InvalidInput(format!("secret bytes are not UTF-8: {err}")))?;
        Ok(Self { bytes })
    }

    /// Returns the plaintext secret; callers must not log or persist it.
    #[must_use]
    pub fn expose(&self) -> &str {
        // Invariant: both constructors validate UTF-8 before storing bytes.
        std::str::from_utf8(&self.bytes).expect("SecretString always holds valid UTF-8")
    }

    /// Returns the length of the secret in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns `true` when the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(secret: String) -> Self {
        Self::new(secret)
    }
}

impl From<&str> for SecretString {
    fn from(secret: &str) -> Self {
        Self::new(secret)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretString({REDACTED})")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // Slice fill is the safe, iterator-based overwrite of the byte buffer.
        self.bytes.fill(0);
        // Keep the wipe from being reordered or elided ahead of drop.
        compiler_fence(Ordering::SeqCst);
    }
}

/// Byte range of one detected credential inside an input string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

/// Whether a byte may appear inside a detected token.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'/' | b'=')
}

/// Whether `needle` occurs anywhere in `haystack`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    find_subslice(haystack, needle).is_some()
}

/// Byte offset of the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Detects a PEM private-key header and, when present, its whole block.
fn pem_private_key_span(bytes: &[u8], index: usize) -> Option<Span> {
    if !bytes[index..].starts_with(PEM_BEGIN) {
        return None;
    }
    let first_newline = bytes[index..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(bytes.len(), |offset| index + offset);
    if !contains_subslice(&bytes[index..first_newline], PRIVATE_KEY_TAIL) {
        return None;
    }
    let end_marker =
        find_subslice(&bytes[first_newline..], PEM_END).map(|offset| first_newline + offset);
    let end = match end_marker {
        Some(marker) => {
            let end_line = bytes[marker..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(bytes.len(), |offset| marker + offset);
            if contains_subslice(&bytes[marker..end_line], PRIVATE_KEY_TAIL) {
                end_line
            } else {
                bytes.len()
            }
        }
        None => bytes.len(),
    };
    Some(Span { start: index, end })
}

/// Detects `Bearer <token>`, case-insensitively, including both words.
fn bearer_span(bytes: &[u8], index: usize) -> Option<Span> {
    let after_word = index + BEARER.len();
    if after_word > bytes.len() || !bytes[index..after_word].eq_ignore_ascii_case(BEARER) {
        return None;
    }
    if index > 0 && bytes[index - 1].is_ascii_alphanumeric() {
        return None;
    }
    let mut cursor = after_word;
    if cursor >= bytes.len() || !matches!(bytes[cursor], b' ' | b'\t') {
        return None;
    }
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
        cursor += 1;
    }
    let token_start = cursor;
    while cursor < bytes.len() && is_token_byte(bytes[cursor]) {
        cursor += 1;
    }
    if cursor == token_start {
        return None;
    }
    Some(Span {
        start: index,
        end: cursor,
    })
}

/// Detects known prefixed token formats such as `sk-` or `ghp_`.
fn token_span(bytes: &[u8], index: usize) -> Option<Span> {
    for &(prefix, min_tail) in TOKEN_PREFIXES {
        if !bytes[index..].starts_with(prefix) {
            continue;
        }
        let after_prefix = index + prefix.len();
        let mut cursor = after_prefix;
        while cursor < bytes.len() && is_token_byte(bytes[cursor]) {
            cursor += 1;
        }
        if cursor >= after_prefix + min_tail {
            return Some(Span {
                start: index,
                end: cursor,
            });
        }
    }
    None
}

/// Returns the credential starting exactly at `index`, if any.
fn secret_at(bytes: &[u8], index: usize) -> Option<Span> {
    pem_private_key_span(bytes, index)
        .or_else(|| bearer_span(bytes, index))
        .or_else(|| token_span(bytes, index))
}

/// Finds the next credential at or after byte offset `from`.
fn find_next_secret(input: &str, from: usize) -> Option<Span> {
    let bytes = input.as_bytes();
    let mut index = from.min(bytes.len());
    while index < bytes.len() {
        if let Some(span) = secret_at(bytes, index) {
            return Some(span);
        }
        index += 1;
    }
    None
}

/// Returns `true` when `input` contains any supported credential format.
pub fn contains_secret(input: &str) -> bool {
    find_next_secret(input, 0).is_some()
}

/// Replaces every detected credential in `input` with [`REDACTED`].
///
/// Non-secret text, including UTF-8 text, is copied through unchanged.
pub fn redact_secrets(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(span) = find_next_secret(input, cursor) {
        // Detectors always consume at least one byte; keep the scan monotonic.
        if span.start < cursor || span.end <= span.start {
            cursor = (cursor + 1).min(input.len());
            continue;
        }
        output.push_str(&input[cursor..span.start]);
        output.push_str(REDACTED);
        cursor = span.end;
    }
    output.push_str(&input[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_never_prints_plaintext() {
        let secret = SecretString::new("hunter2-super-secret");
        let debug = format!("{secret:?}");
        let display = format!("{secret}");
        assert!(debug.contains(REDACTED));
        assert!(display.contains(REDACTED));
        assert!(!debug.contains("hunter2"));
        assert!(!display.contains("hunter2"));
        assert_eq!(display, REDACTED);
    }

    #[test]
    fn secret_string_exposes_plaintext_and_reports_size() {
        let secret = SecretString::new("abc123");
        assert_eq!(secret.expose(), "abc123");
        assert_eq!(secret.len(), 6);
        assert!(!secret.is_empty());
        let empty = SecretString::new("");
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        let converted: SecretString = "from-str".into();
        assert_eq!(converted.expose(), "from-str");
        let owned: SecretString = String::from("from-string").into();
        assert_eq!(owned.expose(), "from-string");
    }

    #[test]
    fn secret_string_from_bytes_validates_utf8() {
        match SecretString::from_bytes(vec![0xff, 0xfe]) {
            Ok(_) => panic!("invalid UTF-8 must be rejected"),
            Err(NaviError::InvalidInput(message)) => assert!(message.contains("UTF-8")),
            Err(other) => panic!("unexpected error: {other}"),
        }
        let valid = SecretString::from_bytes(b"valid".to_vec());
        assert!(matches!(&valid, Ok(secret) if secret.expose() == "valid"));
    }

    #[test]
    fn redacts_openai_and_anthropic_keys() {
        let input = "openai=sk-proj-abcdefghijklmnop anthropic=sk-ant-api03-ABCDEFGHIJKLMNOP";
        let redacted = redact_secrets(input);
        assert!(!redacted.contains("sk-proj"));
        assert!(!redacted.contains("sk-ant"));
        assert!(redacted.contains("openai="));
        assert!(redacted.contains("anthropic="));
        assert_eq!(redacted.matches(REDACTED).count(), 2);
        assert!(contains_secret(input));
    }

    #[test]
    fn redacts_opencode_github_and_aws_keys() {
        let input = "oc_sk_abcdef123456 ghp_abcdef123456 gho_ZZZZ9999 AKIAABCDEFGHIJKLMNOP";
        let redacted = redact_secrets(input);
        for marker in ["oc_sk_", "ghp_", "gho_", "AKIA"] {
            assert!(!redacted.contains(marker), "left behind: {marker}");
        }
        assert_eq!(redacted.matches(REDACTED).count(), 4);
    }

    #[test]
    fn redacts_bearer_token_case_insensitively() {
        let input = "Authorization: Bearer abc.def_ghi-123+/==";
        let redacted = redact_secrets(input);
        assert_eq!(redacted, format!("Authorization: {REDACTED}"));
        assert!(!redacted.contains("abc.def"));
        let lower = "bearer xyz123";
        assert!(!redact_secrets(lower).contains("xyz123"));
        assert!(contains_secret(lower));
    }

    #[test]
    fn redacts_private_key_pem_block() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAL\n-----END RSA PRIVATE KEY-----\n";
        let redacted = redact_secrets(pem);
        assert!(!redacted.contains("MIIBOg"));
        assert!(!redacted.contains("PRIVATE KEY"));
        assert_eq!(redacted, format!("{REDACTED}\n"));
        assert!(contains_secret("-----BEGIN OPENSSH PRIVATE KEY-----"));
    }

    #[test]
    fn contains_secret_ignores_benign_text() {
        assert!(!contains_secret(""));
        assert!(!contains_secret(
            "the quick brown fox jumps over the lazy dog"
        ));
        assert!(!contains_secret(
            "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----"
        ));
        assert!(!contains_secret("Authorization: Bearer"));
        assert!(!contains_secret("sk-"));
        assert!(!contains_secret(
            "this sentence mentions AKIA without a key"
        ));
    }

    #[test]
    fn redacts_multiple_secrets_and_preserves_context() {
        let input = "use sk-AAAA1111 then ghp_BBBB2222 and finally AKIA3333333333333333 done";
        let redacted = redact_secrets(input);
        assert!(redacted.starts_with("use [REDACTED]"));
        assert!(redacted.ends_with("done"));
        assert_eq!(redacted.matches(REDACTED).count(), 3);
        assert!(!redacted.contains("AAAA1111"));
        assert!(!redacted.contains("BBBB2222"));
        assert!(!redacted.contains("AKIA"));
    }

    #[test]
    fn redaction_handles_unicode_and_tightly_adjacent_secrets() {
        let input = "ключ:sk-abc123,ключ2:ghp_xyz789!";
        let redacted = redact_secrets(input);
        assert!(redacted.starts_with("ключ:"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("xyz789"));
        assert_eq!(redacted.matches(REDACTED).count(), 2);
        assert_eq!(redact_secrets(""), "");
    }

    #[test]
    fn redaction_is_idempotent_and_agrees_with_contains_secret() {
        let input = "key=sk-abc123; auth=Bearer def456";
        let once = redact_secrets(input);
        assert_eq!(redact_secrets(&once), once);
        assert!(contains_secret(input));
        assert!(!contains_secret(&once));
        assert_eq!(once.matches(REDACTED).count(), 2);
    }
}
