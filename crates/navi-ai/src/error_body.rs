//! Provider HTTP error body extraction and normalization.
//!
//! Gateways and proxies often answer a non-2xx request with a JSON body while
//! the SDK only surfaces an opaque message such as `"403 status code (no
//! body)"`. The real reason lives in the body. This module probes the common
//! shapes (`{ error: { message, type, code } }`, flat `{ message }`, FastAPI
//! `{ detail }`, and Google error arrays), then falls back to the trimmed
//! plain-text body. Every returned string is truncated on a character
//! boundary, so a multi-megabyte body cannot be retained or printed in full.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Maximum number of characters retained from a body or extracted message.
pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// Maximum number of characters retained from `code` and `error_type`.
const MAX_METADATA_CHARS: usize = 256;

/// Maximum nesting depth walked while probing a parsed error body.
const MAX_EXTRACTION_DEPTH: usize = 6;

/// Normalized provider HTTP error body.
///
/// `status` is supplied by the caller; the remaining fields are derived from
/// the response body. Numeric provider codes are stringified so that a code
/// such as Google's `403` and a string code such as OpenAI's
/// `invalid_api_key` share one representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderErrorBody {
    /// HTTP status code of the failed response.
    pub status: u16,
    /// Provider error code, when the body carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Provider error type: OpenAI's `type` or Google's `status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// Best human-readable message extracted from the body.
    pub message: String,
    /// Trimmed, safely truncated raw body text.
    pub raw: String,
}

/// Extracts a [`ProviderErrorBody`] from an HTTP status and response body.
///
/// Structured JSON is preferred, in this order: a nested `error` object or
/// string, flat `message` / `code` / `type` fields, `detail`, and Google-style
/// `errors` arrays. Bodies that are not JSON objects are treated as plain
/// text. An empty body yields the message `"no body"`.
pub fn parse_provider_error_body(status: u16, body: &str) -> ProviderErrorBody {
    let trimmed = body.trim();
    let raw = truncate_error_text(trimmed, MAX_PROVIDER_ERROR_BODY_CHARS);

    let mut extracted = if trimmed.is_empty() {
        Extracted::default()
    } else {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => extract_from_value(&value, 0),
            Err(_) => {
                let mut fallback = Extracted::default();
                fallback.set_message(trimmed);
                fallback
            }
        }
    };

    let message = match extracted.message.take() {
        Some(text) => truncate_error_text(text.as_str(), MAX_PROVIDER_ERROR_BODY_CHARS),
        None if trimmed.is_empty() => "no body".to_owned(),
        None => raw.clone(),
    };

    ProviderErrorBody {
        status,
        code: extracted
            .code
            .map(|code| truncate_error_text(code.as_str(), MAX_METADATA_CHARS)),
        error_type: extracted
            .error_type
            .map(|kind| truncate_error_text(kind.as_str(), MAX_METADATA_CHARS)),
        message,
        raw,
    }
}

/// Composes a display string from a normalized error.
///
/// The result is `"<status>: <message>"`, with `[error_type]` and `(code)`
/// inserted before the message when they are present. A piece of metadata or
/// the status itself is omitted when the message already contains it, which
/// keeps SDK messages such as `"403 status code (no body)"` readable.
pub fn format_provider_error(err: &ProviderErrorBody) -> String {
    let status = err.status.to_string();
    let message = err.message.trim();
    let mut out = String::with_capacity(status.len() + message.len() + MAX_METADATA_CHARS);

    if !message.starts_with(status.as_str()) {
        out.push_str(status.as_str());
        out.push_str(": ");
    }

    if let Some(kind) = err.error_type.as_deref() {
        let kind = kind.trim();
        if !kind.is_empty() && !message.contains(kind) {
            out.push('[');
            out.push_str(kind);
            out.push_str("] ");
        }
    }
    if let Some(code) = err.code.as_deref() {
        let code = code.trim();
        if !code.is_empty() && !message.contains(code) && !out.contains(code) {
            out.push('(');
            out.push_str(code);
            out.push_str(") ");
        }
    }

    if message.is_empty() {
        out.push_str("no body");
    } else {
        out.push_str(message);
    }
    out
}

/// Truncates `text` to `max_chars` characters without splitting a UTF-8 code
/// point, appending a marker with the number of removed characters.
pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let kept: String = chars.by_ref().take(max_chars).collect();
    let removed = chars.count();
    if removed == 0 {
        kept
    } else {
        format!("{kept}... [truncated {removed} chars]")
    }
}

/// Fields collected from one probe of an error body.
#[derive(Debug, Default)]
struct Extracted {
    message: Option<String>,
    code: Option<String>,
    error_type: Option<String>,
}

impl Extracted {
    fn set_message(&mut self, text: &str) {
        if self.message.is_none() {
            self.message = non_empty(text);
        }
    }

    fn set_code(&mut self, value: &Value) {
        if self.code.is_none() {
            self.code = match value {
                Value::String(text) => non_empty(text.as_str()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            };
        }
    }

    fn set_error_type(&mut self, value: &Value) {
        if self.error_type.is_none() {
            self.error_type = match value {
                Value::String(text) => non_empty(text.as_str()),
                _ => None,
            };
        }
    }

    fn merge(&mut self, other: Extracted) {
        if self.message.is_none() {
            self.message = other.message;
        }
        if self.code.is_none() {
            self.code = other.code;
        }
        if self.error_type.is_none() {
            self.error_type = other.error_type;
        }
    }
}

fn extract_from_value(value: &Value, depth: usize) -> Extracted {
    let mut out = Extracted::default();
    if depth > MAX_EXTRACTION_DEPTH {
        return out;
    }
    match value {
        Value::Object(map) => extract_from_map(map, depth, &mut out),
        Value::Array(items) => extract_from_items(items, depth, &mut out),
        Value::String(text) => out.set_message(text.as_str()),
        Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
    out
}

fn extract_from_map(map: &Map<String, Value>, depth: usize, out: &mut Extracted) {
    // The nested SDK error is the most specific shape, so it wins over fields
    // repeated at the top level.
    if let Some(error @ (Value::Object(_) | Value::Array(_))) = map.get("error") {
        out.merge(extract_from_value(error, depth + 1));
    }

    // Dedicated message fields outrank a bare `error` string, which is often a
    // provider code (OAuth's `error`, for example) rather than prose.
    if let Some(message) = map
        .get("message")
        .or_else(|| map.get("error_message"))
        .or_else(|| map.get("error_description"))
        && let Value::String(text) = message
    {
        out.set_message(text.as_str());
    }
    if let Some(Value::String(text)) = map.get("error") {
        out.set_message(text.as_str());
    }
    if let Some(code) = map.get("code") {
        out.set_code(code);
    }
    if let Some(kind) = map.get("type").or_else(|| map.get("status")) {
        out.set_error_type(kind);
    }

    if let Some(detail) = map.get("detail") {
        extract_detail(detail, depth, out);
    }
    if let Some(errors) = map.get("errors") {
        extract_error_list(errors, out);
    }
}

fn extract_from_items(items: &[Value], depth: usize, out: &mut Extracted) {
    let mut messages: Vec<String> = Vec::new();
    for item in items {
        let part = extract_from_value(item, depth + 1);
        if let Some(code) = part.code
            && out.code.is_none()
        {
            out.code = Some(code);
        }
        if let Some(kind) = part.error_type
            && out.error_type.is_none()
        {
            out.error_type = Some(kind);
        }
        if let Some(message) = part.message
            && !messages.contains(&message)
        {
            messages.push(message);
        }
    }
    if out.message.is_none() && !messages.is_empty() {
        let combined = messages.join("; ");
        out.set_message(combined.as_str());
    }
}

fn extract_detail(value: &Value, depth: usize, out: &mut Extracted) {
    match value {
        Value::String(text) => out.set_message(text.as_str()),
        Value::Array(_) => extract_error_list(value, out),
        Value::Object(_) => out.merge(extract_from_value(value, depth + 1)),
        Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
}

fn extract_error_list(value: &Value, out: &mut Extracted) {
    let Value::Array(items) = value else {
        return;
    };
    let mut messages: Vec<String> = Vec::new();
    for item in items {
        if let Some(message) = message_from_item(item)
            && !messages.contains(&message)
        {
            messages.push(message);
        }
        if let Value::Object(map) = item
            && let Some(code) = map.get("code")
        {
            out.set_code(code);
        }
    }
    if out.message.is_none() && !messages.is_empty() {
        let combined = messages.join("; ");
        out.set_message(combined.as_str());
    }
}

fn message_from_item(item: &Value) -> Option<String> {
    match item {
        Value::String(text) => non_empty(text.as_str()),
        Value::Object(map) => map
            .get("message")
            .or_else(|| map.get("msg"))
            .and_then(non_empty_value)
            .or_else(|| map.get("error").and_then(message_from_item)),
        Value::Array(items) => items.iter().find_map(message_from_item),
        Value::Number(_) | Value::Bool(_) | Value::Null => None,
    }
}

fn non_empty_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text.as_str()),
        _ => None,
    }
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_style_nested_error() {
        let body = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        let parsed = parse_provider_error_body(401, body);

        assert_eq!(parsed.status, 401);
        assert_eq!(parsed.message, "Incorrect API key provided");
        assert_eq!(parsed.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(parsed.code.as_deref(), Some("invalid_api_key"));
        assert!(parsed.raw.contains("Incorrect API key provided"));

        let formatted = format_provider_error(&parsed);
        assert!(formatted.contains("401"));
        assert!(formatted.contains("Incorrect API key provided"));
        assert!(formatted.contains("[invalid_request_error]"));
        assert!(formatted.contains("(invalid_api_key)"));
    }

    #[test]
    fn parses_flat_message_detail_and_error_string() {
        let flat = parse_provider_error_body(400, r#"{"message":"bad request"}"#);
        assert_eq!(flat.message, "bad request");
        assert_eq!(flat.code, None);

        let detail = parse_provider_error_body(404, r#"{"detail":"not found"}"#);
        assert_eq!(detail.message, "not found");

        let detail_list =
            parse_provider_error_body(422, r#"{"detail":[{"msg":"field required"},"bad id"]}"#);
        assert_eq!(detail_list.message, "field required; bad id");

        let error_string = parse_provider_error_body(403, r#"{"error":"blocked by gateway WAF"}"#);
        assert_eq!(error_string.message, "blocked by gateway WAF");
        assert_eq!(
            format_provider_error(&error_string),
            "403: blocked by gateway WAF"
        );
    }

    #[test]
    fn parses_google_error_arrays_and_numeric_code() {
        let body = r#"{"error":{"errors":[{"message":"API key not valid","reason":"API_KEY_INVALID"},{"message":"quota exceeded","reason":"RATE_LIMIT"}],"code":403,"status":"PERMISSION_DENIED","message":"Permission denied"}}"#;
        let parsed = parse_provider_error_body(403, body);

        assert_eq!(parsed.message, "Permission denied");
        assert_eq!(parsed.code.as_deref(), Some("403"));
        assert_eq!(parsed.error_type.as_deref(), Some("PERMISSION_DENIED"));

        let array = parse_provider_error_body(
            400,
            r#"[{"error":{"code":400,"message":"first"}},{"error":{"message":"second"}}]"#,
        );
        assert_eq!(array.message, "first; second");
        assert_eq!(array.code.as_deref(), Some("400"));
    }

    #[test]
    fn trims_plain_text_fallback_and_handles_invalid_json() {
        let plain = parse_provider_error_body(502, "   upstream unavailable \n");
        assert_eq!(plain.message, "upstream unavailable");
        assert_eq!(plain.raw, "upstream unavailable");
        assert_eq!(format_provider_error(&plain), "502: upstream unavailable");

        let malformed = parse_provider_error_body(500, "{not json: gateway exploded");
        assert_eq!(malformed.message, "{not json: gateway exploded");
        assert_eq!(malformed.raw, "{not json: gateway exploded");
    }

    #[test]
    fn empty_body_has_no_body_message() {
        for body in ["", "   \n\t "] {
            let parsed = parse_provider_error_body(504, body);
            assert_eq!(parsed.message, "no body");
            assert_eq!(parsed.raw, "");
            assert_eq!(parsed.code, None);
            assert_eq!(parsed.error_type, None);
            assert_eq!(format_provider_error(&parsed), "504: no body");
        }
    }

    #[test]
    fn scalar_and_empty_json_bodies_fall_back_to_raw_text() {
        for text in ["null", "123", "true", "[]", "{}"] {
            let parsed = parse_provider_error_body(400, text);
            assert_eq!(parsed.message, text);
            assert_eq!(parsed.raw, text);
        }
    }

    #[test]
    fn truncates_long_bodies_on_character_boundaries() {
        let long = "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS + 25);
        let parsed = parse_provider_error_body(500, long.as_str());
        assert!(parsed.raw.ends_with("... [truncated 25 chars]"));
        assert!(parsed.message.ends_with("... [truncated 25 chars]"));
        assert_eq!(
            parsed.raw.chars().count(),
            MAX_PROVIDER_ERROR_BODY_CHARS + "... [truncated 25 chars]".len()
        );
        assert_eq!(
            truncate_error_text("short", MAX_PROVIDER_ERROR_BODY_CHARS),
            "short"
        );

        let unicode = "é".repeat(MAX_PROVIDER_ERROR_BODY_CHARS + 3);
        let unicode_parsed = parse_provider_error_body(500, unicode.as_str());
        assert!(unicode_parsed.raw.ends_with("... [truncated 3 chars]"));
        assert_eq!(
            unicode_parsed
                .raw
                .chars()
                .take(MAX_PROVIDER_ERROR_BODY_CHARS)
                .count(),
            MAX_PROVIDER_ERROR_BODY_CHARS
        );
    }

    #[test]
    fn format_does_not_duplicate_status_or_metadata() {
        let err = ProviderErrorBody {
            status: 403,
            code: Some("invalid_api_key".to_owned()),
            error_type: Some("invalid_request_error".to_owned()),
            message: "403 invalid_api_key: invalid_request_error from gateway".to_owned(),
            raw: String::new(),
        };

        let formatted = format_provider_error(&err);
        assert_eq!(formatted, err.message);
        assert_eq!(formatted.matches("invalid_api_key").count(), 1);
        assert_eq!(formatted.matches("invalid_request_error").count(), 1);
    }

    #[test]
    fn deeply_nested_bodies_are_bounded() {
        let mut body = String::from("{\"error\":");
        for _ in 0..40 {
            body.push_str("{\"error\":");
        }
        body.push_str("{\"message\":\"buried\"}");
        for _ in 0..40 {
            body.push('}');
        }
        body.push('}');

        let parsed = parse_provider_error_body(500, body.as_str());
        assert_eq!(parsed.message, parsed.raw);
        assert_eq!(parsed.code, None);
    }

    #[test]
    fn extracts_oauth_error_description() {
        let parsed = parse_provider_error_body(
            401,
            r#"{"error":"invalid_client","error_description":"Client authentication failed"}"#,
        );
        assert_eq!(parsed.message, "Client authentication failed");
        assert_eq!(
            format_provider_error(&parsed),
            "401: Client authentication failed"
        );
    }

    #[test]
    fn caps_metadata_and_roundtrips_through_serde() {
        let long = "m".repeat(MAX_METADATA_CHARS + 10);
        let body = format!(r#"{{"message":"failed","code":"{long}","type":"{long}"}}"#);
        let parsed = parse_provider_error_body(400, body.as_str());
        let expected = format!("{}... [truncated 10 chars]", "m".repeat(MAX_METADATA_CHARS));
        assert_eq!(parsed.message, "failed");
        assert_eq!(parsed.code.as_deref(), Some(expected.as_str()));
        assert_eq!(parsed.error_type.as_deref(), Some(expected.as_str()));

        let encoded = serde_json::to_string(&parsed).expect("serialize");
        let decoded: ProviderErrorBody = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, parsed);

        let minimal: ProviderErrorBody =
            serde_json::from_str(r#"{"status":500,"message":"boom","raw":"boom"}"#)
                .expect("deserialize minimal");
        assert_eq!(minimal.code, None);
        assert_eq!(minimal.error_type, None);
    }
}
