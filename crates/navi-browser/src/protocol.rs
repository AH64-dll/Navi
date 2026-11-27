//! NAVI protocol v1 frame model, receiver-side validation, and JSON codec.
//!
//! Ported from `packages/navi/src/protocol/{limits,types,validate,codec}.ts`.
//! The wire unit is one UTF-8 JSON object; every untrusted value passes through
//! [`validate_frame`] before it becomes a typed [`ProtocolFrame`].

use std::borrow::Borrow;

use navi_types::{NaviError, NaviResult};
use serde::Serialize;
use serde_json::{Map, Value};

/// Shared protocol version. Bumped only for a breaking frame or payload change.
pub const NAVI_PROTOCOL_VERSION: u64 = 1;

/// Largest serialized frame accepted in either direction, in UTF-8 bytes.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Largest serialized observation payload, in UTF-8 bytes.
pub const MAX_OBSERVATION_BYTES: usize = 16 * 1024;

/// Largest number of DOM nodes returned in one observation.
pub const MAX_NODES: usize = 100;

/// Largest number of UTF-16 code units kept per node `name` and `text`.
pub const MAX_TEXT_CHARS: usize = 200;

/// Collector element visit budget before a capture reports truncation.
pub const MAX_VISITED_ELEMENTS: usize = 1500;

/// Browser operations in flight per connection.
pub const MAX_INFLIGHT_BROWSER_OPS: usize = 1;

/// Outbound queue bounds per direction.
pub const MAX_QUEUE_FRAMES: usize = 16;
pub const MAX_QUEUE_BYTES: usize = 2 * MAX_FRAME_BYTES;

/// Largest single scroll delta accepted in a `scroll` request, in CSS pixels.
pub const MAX_SCROLL_DELTA: i64 = 1000;

/// Bounded identifier and message lengths (UTF-16 code units, as in JavaScript).
pub const MAX_ID_CHARS: usize = 64;
pub const MAX_VERSION_CHARS: usize = 32;
pub const MAX_URL_CHARS: usize = 512;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_ERROR_MESSAGE_CHARS: usize = 200;

/// Highest accepted sequence number. Sequence numbers are strictly increasing.
pub const MAX_SEQUENCE: u64 = (1_u64 << 53) - 1;

/// Capabilities a user grant may enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Observe,
    Focus,
    Scroll,
}

const CAPABILITIES: [Capability; 3] = [Capability::Observe, Capability::Focus, Capability::Scroll];

/// Returns whether `s` is one of the protocol capability tokens.
pub fn is_capability(s: &str) -> bool {
    capability_index(s).is_some()
}

fn capability_index(s: &str) -> Option<usize> {
    match s {
        "observe" => Some(0),
        "focus" => Some(1),
        "scroll" => Some(2),
        _ => None,
    }
}

/// Sender role. Both roles send the same frame shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerKind {
    Cli,
    Extension,
}

/// Peer identity carried by the handshake frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub kind: PeerKind,
    pub version: String,
}

/// One user grant plus the exact top-frame document it applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub grant_id: String,
    pub tab_id: u64,
    pub document_id: String,
    pub page_epoch: u64,
}

/// Grant state for a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub grant_id: String,
    pub binding: Binding,
    pub capabilities: Vec<Capability>,
}

/// Bounding box of a collected element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DomRect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// One collected top-frame element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomNode {
    #[serde(rename = "ref")]
    pub reference: String,
    pub depth: u64,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub interactive: bool,
    pub editable: bool,
    pub offscreen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<DomRect>,
}

/// Capture regions and budgets that produced partial coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OmittedRegion {
    Subframes,
    Shadow,
    Canvas,
    Offscreen,
    NodeLimit,
    ByteLimit,
}

/// Bounded snapshot of the granted top-frame document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub observation_id: String,
    pub binding: Binding,
    pub captured_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub url: String,
    pub nodes: Vec<DomNode>,
    pub visited: u64,
    pub truncated: bool,
    pub omitted: Vec<OmittedRegion>,
}

/// Alignment used when scrolling to a referenced element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollAlign {
    Start,
    Center,
    End,
}

/// Whether an interaction effect was applied or has an unknown outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectOutcome {
    Applied,
    Unknown,
}

/// Reason a grant changed during a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GrantChangeReason {
    Attached,
    Navigated,
    Closed,
    Detached,
    Expired,
    WorkerRestart,
    Disconnected,
}

/// Reason a connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ByeReason {
    Closed,
    Detached,
    Idle,
    ProtocolError,
    Disconnected,
}

/// The only event kind in protocol v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EventKind {
    #[serde(rename = "binding.changed")]
    BindingChanged,
}

/// Event payload: `grant: null` invalidates every observation for that document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventPayload {
    pub grant: Option<Grant>,
    pub reason: GrantChangeReason,
}

/// Codes a peer may put in a wire error. Transport-local codes never travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WireErrorCode {
    VersionMismatch,
    NotHelloFirst,
    DuplicateHello,
    ConnectionMismatch,
    SequenceGap,
    BadEnvelope,
    Backpressure,
    UnsupportedOp,
    UnknownRequest,
    DuplicateRequest,
    TooManyInflight,
    BadArgument,
    NotAttached,
    BindingStale,
    StaleObservation,
    UnknownRef,
    CaptureFailed,
    TooLarge,
    Busy,
    TabNotActive,
    DeadlineExceeded,
    Cancelled,
    OutcomeUnknown,
    Internal,
}

/// Bounded, redacted error carried on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WireError {
    pub code: WireErrorCode,
    pub message: String,
    pub retryable: bool,
}

/// Payload of a `request` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum RequestPayload {
    Observe {
        binding: Binding,
    },
    Focus {
        binding: Binding,
        observation_id: String,
        #[serde(rename = "ref")]
        reference: String,
    },
    Scroll {
        binding: Binding,
        observation_id: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        align: Option<ScrollAlign>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta_x: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delta_y: Option<i64>,
    },
    Cancel {
        request_id: String,
    },
}

/// Payload of a successful `result` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum ResultPayload {
    Observe {
        observation: Observation,
    },
    Focus {
        outcome: EffectOutcome,
    },
    Scroll {
        outcome: EffectOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        scroll_x: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scroll_y: Option<i64>,
    },
    Cancel {
        cancelled: bool,
        already_terminal: bool,
    },
}

/// One validated protocol frame. Serialization is internally tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum ProtocolFrame {
    #[serde(rename = "hello")]
    Hello {
        v: u64,
        seq: u64,
        peer: PeerInfo,
        capabilities: Vec<Capability>,
    },
    #[serde(rename = "hello.ack")]
    HelloAck {
        v: u64,
        seq: u64,
        connection_id: String,
        peer: PeerInfo,
        capabilities: Vec<Capability>,
    },
    #[serde(rename = "hello.error")]
    HelloError { v: u64, seq: u64, error: WireError },
    #[serde(rename = "request")]
    Request {
        v: u64,
        seq: u64,
        connection_id: String,
        request_id: String,
        payload: RequestPayload,
    },
    #[serde(rename = "result")]
    ResultOk {
        v: u64,
        seq: u64,
        connection_id: String,
        request_id: String,
        ok: bool,
        payload: ResultPayload,
    },
    #[serde(rename = "result")]
    ResultErr {
        v: u64,
        seq: u64,
        connection_id: String,
        request_id: String,
        ok: bool,
        error: WireError,
    },
    #[serde(rename = "event")]
    Event {
        v: u64,
        seq: u64,
        connection_id: String,
        kind: EventKind,
        payload: EventPayload,
    },
    #[serde(rename = "bye")]
    Bye {
        v: u64,
        seq: u64,
        connection_id: String,
        reason: ByeReason,
    },
}

// ---------------------------------------------------------------------------
// Error reporting and primitive validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum ValidationCode {
    BadJson,
    NotJsonObject,
    BadEnvelope,
    TooLarge,
    BadArgument,
    UnsupportedOp,
    VersionMismatch,
    EnvelopeTooLarge,
}

impl ValidationCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::BadJson => "BAD_JSON",
            Self::NotJsonObject => "NOT_JSON_OBJECT",
            Self::BadEnvelope => "BAD_ENVELOPE",
            Self::TooLarge => "TOO_LARGE",
            Self::BadArgument => "BAD_ARGUMENT",
            Self::UnsupportedOp => "UNSUPPORTED_OP",
            Self::VersionMismatch => "VERSION_MISMATCH",
            Self::EnvelopeTooLarge => "ENVELOPE_TOO_LARGE",
        }
    }
}

fn fail(code: ValidationCode, path: &str, message: &str) -> NaviError {
    let location = if path.is_empty() { "frame" } else { path };
    NaviError::Protocol(format!("{}: {location}: {message}", code.as_str()))
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}

const FRAME_TYPES: &[&str] = &[
    "hello",
    "hello.ack",
    "hello.error",
    "request",
    "result",
    "event",
    "bye",
];
const OMITTED_REGIONS: &[(&str, OmittedRegion)] = &[
    ("subframes", OmittedRegion::Subframes),
    ("shadow", OmittedRegion::Shadow),
    ("canvas", OmittedRegion::Canvas),
    ("offscreen", OmittedRegion::Offscreen),
    ("node-limit", OmittedRegion::NodeLimit),
    ("byte-limit", OmittedRegion::ByteLimit),
];
const SCROLL_ALIGNS: &[(&str, ScrollAlign)] = &[
    ("start", ScrollAlign::Start),
    ("center", ScrollAlign::Center),
    ("end", ScrollAlign::End),
];
const GRANT_CHANGE_REASONS: &[(&str, GrantChangeReason)] = &[
    ("attached", GrantChangeReason::Attached),
    ("navigated", GrantChangeReason::Navigated),
    ("closed", GrantChangeReason::Closed),
    ("detached", GrantChangeReason::Detached),
    ("expired", GrantChangeReason::Expired),
    ("worker-restart", GrantChangeReason::WorkerRestart),
    ("disconnected", GrantChangeReason::Disconnected),
];
const BYE_REASONS: &[(&str, ByeReason)] = &[
    ("closed", ByeReason::Closed),
    ("detached", ByeReason::Detached),
    ("idle", ByeReason::Idle),
    ("protocol-error", ByeReason::ProtocolError),
    ("disconnected", ByeReason::Disconnected),
];
const OUTCOMES: &[(&str, EffectOutcome)] = &[
    ("applied", EffectOutcome::Applied),
    ("unknown", EffectOutcome::Unknown),
];
const PEER_KINDS: &[(&str, PeerKind)] =
    &[("cli", PeerKind::Cli), ("extension", PeerKind::Extension)];
const EVENT_KINDS: &[(&str, EventKind)] = &[("binding.changed", EventKind::BindingChanged)];
const WIRE_ERROR_CODES: &[(&str, WireErrorCode)] = &[
    ("VERSION_MISMATCH", WireErrorCode::VersionMismatch),
    ("NOT_HELLO_FIRST", WireErrorCode::NotHelloFirst),
    ("DUPLICATE_HELLO", WireErrorCode::DuplicateHello),
    ("CONNECTION_MISMATCH", WireErrorCode::ConnectionMismatch),
    ("SEQUENCE_GAP", WireErrorCode::SequenceGap),
    ("BAD_ENVELOPE", WireErrorCode::BadEnvelope),
    ("BACKPRESSURE", WireErrorCode::Backpressure),
    ("UNSUPPORTED_OP", WireErrorCode::UnsupportedOp),
    ("UNKNOWN_REQUEST", WireErrorCode::UnknownRequest),
    ("DUPLICATE_REQUEST", WireErrorCode::DuplicateRequest),
    ("TOO_MANY_INFLIGHT", WireErrorCode::TooManyInflight),
    ("BAD_ARGUMENT", WireErrorCode::BadArgument),
    ("NOT_ATTACHED", WireErrorCode::NotAttached),
    ("BINDING_STALE", WireErrorCode::BindingStale),
    ("STALE_OBSERVATION", WireErrorCode::StaleObservation),
    ("UNKNOWN_REF", WireErrorCode::UnknownRef),
    ("CAPTURE_FAILED", WireErrorCode::CaptureFailed),
    ("TOO_LARGE", WireErrorCode::TooLarge),
    ("BUSY", WireErrorCode::Busy),
    ("TAB_NOT_ACTIVE", WireErrorCode::TabNotActive),
    ("DEADLINE_EXCEEDED", WireErrorCode::DeadlineExceeded),
    ("CANCELLED", WireErrorCode::Cancelled),
    ("OUTCOME_UNKNOWN", WireErrorCode::OutcomeUnknown),
    ("INTERNAL", WireErrorCode::Internal),
];

/// UTF-8 byte length of the exact JSON serialization of `value`.
fn json_byte_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| text.len())
}

fn expect_record<'a>(value: &'a Value, path: &str) -> NaviResult<&'a Map<String, Value>> {
    value.as_object().ok_or_else(|| {
        fail(
            ValidationCode::NotJsonObject,
            path,
            "Expected a JSON object",
        )
    })
}

fn expect_keys(
    obj: &Map<String, Value>,
    allowed: &[&str],
    required: &[&str],
    path: &str,
) -> NaviResult<()> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(fail(
                ValidationCode::BadEnvelope,
                &join_path(path, key),
                "Unknown field",
            ));
        }
    }
    for key in required {
        if !obj.contains_key(*key) {
            return Err(fail(
                ValidationCode::BadEnvelope,
                &join_path(path, key),
                "Missing field",
            ));
        }
    }
    Ok(())
}

fn expect_string(
    value: Option<&Value>,
    path: &str,
    max_chars: usize,
    allow_empty: bool,
) -> NaviResult<String> {
    let Some(Value::String(text)) = value else {
        return Err(fail(ValidationCode::BadEnvelope, path, "Expected a string"));
    };
    if !allow_empty && text.is_empty() {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Expected a non-empty string",
        ));
    }
    if text.encode_utf16().count() > max_chars {
        return Err(fail(
            ValidationCode::TooLarge,
            path,
            &format!("String exceeds {max_chars} characters"),
        ));
    }
    Ok(text.clone())
}

fn expect_optional_string(
    value: Option<&Value>,
    path: &str,
    max_chars: usize,
) -> NaviResult<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(expect_string(Some(value), path, max_chars, true)?)),
    }
}

/// Rejects C0/C1 controls, DEL, and bidi overrides/isolates.
fn has_forbidden_characters(text: &str) -> bool {
    text.chars().any(|character| {
        let code = u32::from(character);
        code <= 0x1F
            || (0x7F..=0x9F).contains(&code)
            || (0x202A..=0x202E).contains(&code)
            || (0x2066..=0x2069).contains(&code)
    })
}

fn expect_safe_text(
    value: Option<&Value>,
    path: &str,
    max_chars: usize,
    allow_empty: bool,
) -> NaviResult<String> {
    let text = expect_string(value, path, max_chars, allow_empty)?;
    if has_forbidden_characters(&text) {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Text contains control characters",
        ));
    }
    Ok(text)
}

fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

fn expect_id(value: Option<&Value>, path: &str) -> NaviResult<String> {
    let id = expect_string(value, path, MAX_ID_CHARS, false)?;
    if !is_id(&id) {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Id must use ASCII letters, digits, dot, underscore, colon or hyphen",
        ));
    }
    Ok(id)
}

fn expect_tag(value: Option<&Value>, path: &str) -> NaviResult<String> {
    let tag = expect_string(value, path, MAX_ID_CHARS, false)?;
    let mut characters = tag.chars();
    let valid = characters.next().is_some_and(|c| c.is_ascii_lowercase())
        && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Tag must be a lowercase element name",
        ));
    }
    Ok(tag)
}

fn expect_role(value: &Value, path: &str) -> NaviResult<String> {
    let role = expect_string(Some(value), path, MAX_ID_CHARS, false)?;
    let mut characters = role.chars();
    let valid = characters.next().is_some_and(|c| c.is_ascii_lowercase())
        && characters.all(|c| c.is_ascii_lowercase() || c == '-');
    if !valid {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Role must be a lowercase ARIA token",
        ));
    }
    Ok(role)
}

fn expect_version(value: Option<&Value>, path: &str) -> NaviResult<String> {
    let version = expect_string(value, path, MAX_VERSION_CHARS, false)?;
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '+' | '-'))
    {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Version must be an ASCII token",
        ));
    }
    Ok(version)
}

fn expect_enum<T: Copy>(value: Option<&Value>, path: &str, allowed: &[(&str, T)]) -> NaviResult<T> {
    let Some(text) = value.and_then(Value::as_str) else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Expected a string enum value",
        ));
    };
    for (name, item) in allowed {
        if *name == text {
            return Ok(*item);
        }
    }
    Err(fail(
        ValidationCode::BadEnvelope,
        path,
        "Unknown enum value",
    ))
}

fn expect_bool(value: Option<&Value>, path: &str) -> NaviResult<bool> {
    value
        .and_then(Value::as_bool)
        .ok_or_else(|| fail(ValidationCode::BadEnvelope, path, "Expected a boolean"))
}

/// Accepts JSON integers and integral floats inside the JavaScript safe range.
fn safe_int(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_u64() {
        return i64::try_from(number)
            .ok()
            .filter(|number| *number <= MAX_SEQUENCE as i64);
    }
    if let Some(number) = value.as_i64() {
        return (number >= -(MAX_SEQUENCE as i64)).then_some(number);
    }
    value.as_f64().and_then(|number| {
        if number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SEQUENCE as f64 {
            Some(number as i64)
        } else {
            None
        }
    })
}

fn expect_int(value: Option<&Value>, path: &str, min: i64, max: i64) -> NaviResult<i64> {
    let Some(number) = value.and_then(safe_int) else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Expected a safe integer",
        ));
    };
    if !(min..=max).contains(&number) {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            &format!("Expected an integer between {min} and {max}"),
        ));
    }
    Ok(number)
}

fn validate_port<'a>(port: &'a str, path: &str) -> NaviResult<&'a str> {
    if port.is_empty()
        || !port.bytes().all(|byte| byte.is_ascii_digit())
        || (port.len() > 1 && port.starts_with('0'))
    {
        return Err(fail(ValidationCode::BadEnvelope, path, "Invalid URL port"));
    }
    let number: u32 = port
        .parse()
        .map_err(|_| fail(ValidationCode::BadEnvelope, path, "Invalid URL port"))?;
    if number > u32::from(u16::MAX) {
        return Err(fail(ValidationCode::BadEnvelope, path, "Invalid URL port"));
    }
    Ok(port)
}

/// Splits a canonical http(s) authority into host (with brackets for IPv6) and port.
fn split_authority<'a>(authority: &'a str, path: &str) -> NaviResult<(&'a str, Option<&'a str>)> {
    if let Some(inner) = authority.strip_prefix('[') {
        let Some((host, tail)) = inner.split_once(']') else {
            return Err(fail(
                ValidationCode::BadEnvelope,
                path,
                "URL must be a canonical origin and path",
            ));
        };
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Err(fail(
                ValidationCode::BadEnvelope,
                path,
                "URL must be a canonical origin and path",
            ));
        }
        let port = match tail {
            "" => None,
            tail if tail.starts_with(':') => Some(validate_port(&tail[1..], path)?),
            _ => {
                return Err(fail(
                    ValidationCode::BadEnvelope,
                    path,
                    "URL must be a canonical origin and path",
                ));
            }
        };
        let host_end = authority.len() - tail.len();
        return Ok((&authority[..host_end], port));
    }
    if authority.is_empty() {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL must be a canonical origin and path",
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(validate_port(port, path)?)),
        None => (authority, None),
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL must be a canonical origin and path",
        ));
    }
    Ok((host, port))
}

/// Accepts only an absolute, canonical, credential-free http(s) URL with no query or fragment.
fn expect_display_url(value: Option<&Value>, path: &str) -> NaviResult<String> {
    let text = expect_string(value, path, MAX_URL_CHARS, false)?;
    if has_forbidden_characters(&text) {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL contains control characters",
        ));
    }
    let (scheme, rest) = if let Some(rest) = text.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = text.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL must be an absolute http or https URL",
        ));
    };
    if rest.contains('@') {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL must not contain credentials",
        ));
    }
    let (authority, pathname) = rest.split_once('/').unwrap_or((rest, ""));
    if pathname.is_empty()
        || pathname.contains('?')
        || pathname.contains('#')
        || pathname.contains('\\')
    {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL must be a canonical origin and path",
        ));
    }
    let (host, port) = split_authority(authority, path)?;
    let default_port = if scheme == "https" { "443" } else { "80" };
    let canonical_authority = match port {
        Some(port) if port != default_port => format!("{host}:{port}"),
        _ => host.to_string(),
    };
    if text != format!("{scheme}://{canonical_authority}/{pathname}") {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "URL must be a canonical origin and path",
        ));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Composite validation
// ---------------------------------------------------------------------------

fn expect_capability_list(value: Option<&Value>, path: &str) -> NaviResult<Vec<Capability>> {
    let Some(Value::Array(items)) = value else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Expected an array of capabilities",
        ));
    };
    if items.is_empty() {
        return Err(fail(
            ValidationCode::BadEnvelope,
            path,
            "Expected at least one capability",
        ));
    }
    let mut seen = [false; 3];
    let mut capabilities = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let Some(text) = item.as_str() else {
            return Err(fail(
                ValidationCode::BadEnvelope,
                &item_path,
                "Unknown capability",
            ));
        };
        let Some(position) = capability_index(text) else {
            return Err(fail(
                ValidationCode::BadEnvelope,
                &item_path,
                "Unknown capability",
            ));
        };
        if seen[position] {
            return Err(fail(
                ValidationCode::BadEnvelope,
                &item_path,
                "Duplicate capability",
            ));
        }
        seen[position] = true;
        capabilities.push(CAPABILITIES[position]);
    }
    Ok(capabilities)
}

fn expect_binding(value: Option<&Value>, path: &str) -> NaviResult<Binding> {
    const ALLOWED: &[&str] = &["grantId", "tabId", "documentId", "pageEpoch"];
    let value = value.ok_or_else(|| fail(ValidationCode::BadEnvelope, path, "Missing field"))?;
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, ALLOWED, path)?;
    Ok(Binding {
        grant_id: expect_id(obj.get("grantId"), &join_path(path, "grantId"))?,
        tab_id: expect_int(
            obj.get("tabId"),
            &join_path(path, "tabId"),
            0,
            MAX_SEQUENCE as i64,
        )? as u64,
        document_id: expect_id(obj.get("documentId"), &join_path(path, "documentId"))?,
        page_epoch: expect_int(
            obj.get("pageEpoch"),
            &join_path(path, "pageEpoch"),
            0,
            MAX_SEQUENCE as i64,
        )? as u64,
    })
}

fn expect_grant(value: &Value, path: &str) -> NaviResult<Grant> {
    const ALLOWED: &[&str] = &["grantId", "binding", "capabilities"];
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, ALLOWED, path)?;
    let grant_id = expect_id(obj.get("grantId"), &join_path(path, "grantId"))?;
    let binding = expect_binding(obj.get("binding"), &join_path(path, "binding"))?;
    let capabilities =
        expect_capability_list(obj.get("capabilities"), &join_path(path, "capabilities"))?;
    if grant_id != binding.grant_id {
        return Err(fail(
            ValidationCode::BadArgument,
            &join_path(path, "grantId"),
            "Grant id must match its binding",
        ));
    }
    Ok(Grant {
        grant_id,
        binding,
        capabilities,
    })
}

fn expect_rect(value: &Value, path: &str) -> NaviResult<DomRect> {
    const ALLOWED: &[&str] = &["x", "y", "w", "h"];
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, ALLOWED, path)?;
    Ok(DomRect {
        x: expect_int(obj.get("x"), &join_path(path, "x"), -1_000_000, 1_000_000)?,
        y: expect_int(obj.get("y"), &join_path(path, "y"), -1_000_000, 1_000_000)?,
        w: expect_int(obj.get("w"), &join_path(path, "w"), 0, 1_000_000)?,
        h: expect_int(obj.get("h"), &join_path(path, "h"), 0, 1_000_000)?,
    })
}

fn expect_node(value: &Value, path: &str) -> NaviResult<DomNode> {
    const ALLOWED: &[&str] = &[
        "ref",
        "depth",
        "tag",
        "role",
        "name",
        "text",
        "interactive",
        "editable",
        "offscreen",
        "rect",
    ];
    const REQUIRED: &[&str] = &[
        "ref",
        "depth",
        "tag",
        "interactive",
        "editable",
        "offscreen",
    ];
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, REQUIRED, path)?;
    let editable = expect_bool(obj.get("editable"), &join_path(path, "editable"))?;
    if editable && obj.contains_key("text") {
        return Err(fail(
            ValidationCode::BadEnvelope,
            &join_path(path, "text"),
            "Editable elements must not carry text",
        ));
    }
    let role = match obj.get("role") {
        Some(value) => Some(expect_role(value, &join_path(path, "role"))?),
        None => None,
    };
    let rect = match obj.get("rect") {
        Some(value) => Some(expect_rect(value, &join_path(path, "rect"))?),
        None => None,
    };
    Ok(DomNode {
        reference: expect_id(obj.get("ref"), &join_path(path, "ref"))?,
        depth: expect_int(
            obj.get("depth"),
            &join_path(path, "depth"),
            0,
            MAX_NODES as i64,
        )? as u64,
        tag: expect_tag(obj.get("tag"), &join_path(path, "tag"))?,
        role,
        name: expect_optional_string(obj.get("name"), &join_path(path, "name"), MAX_TEXT_CHARS)?,
        text: expect_optional_string(obj.get("text"), &join_path(path, "text"), MAX_TEXT_CHARS)?,
        interactive: expect_bool(obj.get("interactive"), &join_path(path, "interactive"))?,
        editable,
        offscreen: expect_bool(obj.get("offscreen"), &join_path(path, "offscreen"))?,
        rect,
    })
}

fn expect_observation(value: &Value, path: &str) -> NaviResult<Observation> {
    const ALLOWED: &[&str] = &[
        "observationId",
        "binding",
        "capturedAt",
        "title",
        "url",
        "nodes",
        "visited",
        "truncated",
        "omitted",
    ];
    const REQUIRED: &[&str] = &[
        "observationId",
        "binding",
        "capturedAt",
        "url",
        "nodes",
        "visited",
        "truncated",
        "omitted",
    ];
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, REQUIRED, path)?;
    let title = match obj.get("title") {
        Some(value) => Some(expect_safe_text(
            Some(value),
            &join_path(path, "title"),
            MAX_TITLE_CHARS,
            true,
        )?),
        None => None,
    };
    let nodes_path = join_path(path, "nodes");
    let Some(nodes_value) = obj.get("nodes") else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            &nodes_path,
            "Missing field",
        ));
    };
    let nodes = nodes_value.as_array().ok_or_else(|| {
        fail(
            ValidationCode::BadEnvelope,
            &nodes_path,
            "Expected an array of nodes",
        )
    })?;
    if nodes.len() > MAX_NODES {
        return Err(fail(
            ValidationCode::TooLarge,
            &nodes_path,
            &format!("Observation exceeds {MAX_NODES} nodes"),
        ));
    }
    let mut collected = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        collected.push(expect_node(node, &format!("{nodes_path}[{index}]"))?);
    }
    let visited = expect_int(
        obj.get("visited"),
        &join_path(path, "visited"),
        0,
        MAX_VISITED_ELEMENTS as i64,
    )? as u64;
    if visited < collected.len() as u64 {
        return Err(fail(
            ValidationCode::BadEnvelope,
            &join_path(path, "visited"),
            "Visited count cannot be smaller than the returned node count",
        ));
    }
    let omitted_path = join_path(path, "omitted");
    let Some(omitted_value) = obj.get("omitted") else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            &omitted_path,
            "Missing field",
        ));
    };
    let omitted = omitted_value.as_array().ok_or_else(|| {
        fail(
            ValidationCode::BadEnvelope,
            &omitted_path,
            "Expected an array of regions",
        )
    })?;
    let mut regions = Vec::with_capacity(omitted.len());
    for (index, region) in omitted.iter().enumerate() {
        regions.push(expect_enum(
            Some(region),
            &format!("{omitted_path}[{index}]"),
            OMITTED_REGIONS,
        )?);
    }
    if json_byte_len(value) > MAX_OBSERVATION_BYTES {
        return Err(fail(
            ValidationCode::TooLarge,
            path,
            &format!("Observation exceeds {MAX_OBSERVATION_BYTES} bytes"),
        ));
    }
    Ok(Observation {
        observation_id: expect_id(obj.get("observationId"), &join_path(path, "observationId"))?,
        binding: expect_binding(obj.get("binding"), &join_path(path, "binding"))?,
        captured_at: expect_int(
            obj.get("capturedAt"),
            &join_path(path, "capturedAt"),
            0,
            MAX_SEQUENCE as i64,
        )? as u64,
        title,
        url: expect_display_url(obj.get("url"), &join_path(path, "url"))?,
        nodes: collected,
        visited,
        truncated: expect_bool(obj.get("truncated"), &join_path(path, "truncated"))?,
        omitted: regions,
    })
}

fn expect_error(value: Option<&Value>, path: &str) -> NaviResult<WireError> {
    const ALLOWED: &[&str] = &["code", "message", "retryable"];
    let value = value.ok_or_else(|| fail(ValidationCode::BadEnvelope, path, "Missing field"))?;
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, ALLOWED, path)?;
    Ok(WireError {
        code: expect_enum(obj.get("code"), &join_path(path, "code"), WIRE_ERROR_CODES).map_err(
            |_| {
                fail(
                    ValidationCode::BadEnvelope,
                    &join_path(path, "code"),
                    "Unknown or transport-local error code",
                )
            },
        )?,
        message: expect_safe_text(
            obj.get("message"),
            &join_path(path, "message"),
            MAX_ERROR_MESSAGE_CHARS,
            false,
        )?,
        retryable: expect_bool(obj.get("retryable"), &join_path(path, "retryable"))?,
    })
}

fn expect_peer(value: Option<&Value>, path: &str) -> NaviResult<PeerInfo> {
    const ALLOWED: &[&str] = &["kind", "version"];
    let value = value.ok_or_else(|| fail(ValidationCode::BadEnvelope, path, "Missing field"))?;
    let obj = expect_record(value, path)?;
    expect_keys(obj, ALLOWED, ALLOWED, path)?;
    Ok(PeerInfo {
        kind: expect_enum(obj.get("kind"), &join_path(path, "kind"), PEER_KINDS)?,
        version: expect_version(obj.get("version"), &join_path(path, "version"))?,
    })
}

fn expect_header(frame: &Map<String, Value>) -> NaviResult<(u64, String)> {
    if frame.get("v").and_then(safe_int) != Some(NAVI_PROTOCOL_VERSION as i64) {
        let shown = frame
            .get("v")
            .map_or_else(|| "undefined".to_string(), ToString::to_string);
        return Err(fail(
            ValidationCode::VersionMismatch,
            "v",
            &format!("Unsupported protocol version {shown}"),
        ));
    }
    let seq = expect_int(frame.get("seq"), "seq", 1, MAX_SEQUENCE as i64)? as u64;
    let Some(frame_type) = frame.get("type").and_then(Value::as_str) else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            "type",
            "Unknown frame type",
        ));
    };
    if !FRAME_TYPES.contains(&frame_type) {
        return Err(fail(
            ValidationCode::BadEnvelope,
            "type",
            "Unknown frame type",
        ));
    }
    Ok((seq, frame_type.to_string()))
}

fn expect_request_payload(value: Option<&Value>) -> NaviResult<RequestPayload> {
    const ALLOWED_SCROLL: &[&str] = &[
        "op",
        "binding",
        "observationId",
        "ref",
        "align",
        "deltaX",
        "deltaY",
    ];
    const REQUIRED_SCROLL: &[&str] = &["op", "binding", "observationId"];
    let value =
        value.ok_or_else(|| fail(ValidationCode::BadEnvelope, "payload", "Missing field"))?;
    let obj = expect_record(value, "payload")?;
    let Some(op) = obj.get("op").and_then(Value::as_str) else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            "payload.op",
            "Expected an operation",
        ));
    };
    match op {
        "observe" => {
            const ALLOWED: &[&str] = &["op", "binding"];
            expect_keys(obj, ALLOWED, ALLOWED, "payload")?;
            Ok(RequestPayload::Observe {
                binding: expect_binding(obj.get("binding"), "payload.binding")?,
            })
        }
        "focus" => {
            const ALLOWED: &[&str] = &["op", "binding", "observationId", "ref"];
            expect_keys(obj, ALLOWED, ALLOWED, "payload")?;
            Ok(RequestPayload::Focus {
                binding: expect_binding(obj.get("binding"), "payload.binding")?,
                observation_id: expect_id(obj.get("observationId"), "payload.observationId")?,
                reference: expect_id(obj.get("ref"), "payload.ref")?,
            })
        }
        "scroll" => {
            expect_keys(obj, ALLOWED_SCROLL, REQUIRED_SCROLL, "payload")?;
            let binding = expect_binding(obj.get("binding"), "payload.binding")?;
            let observation_id = expect_id(obj.get("observationId"), "payload.observationId")?;
            let reference = match obj.get("ref") {
                Some(value) => Some(expect_id(Some(value), "payload.ref")?),
                None => None,
            };
            let align = match obj.get("align") {
                Some(value) => Some(expect_enum(Some(value), "payload.align", SCROLL_ALIGNS)?),
                None => None,
            };
            let delta_x = match obj.get("deltaX") {
                Some(value) => Some(expect_int(
                    Some(value),
                    "payload.deltaX",
                    -MAX_SCROLL_DELTA,
                    MAX_SCROLL_DELTA,
                )?),
                None => None,
            };
            let delta_y = match obj.get("deltaY") {
                Some(value) => Some(expect_int(
                    Some(value),
                    "payload.deltaY",
                    -MAX_SCROLL_DELTA,
                    MAX_SCROLL_DELTA,
                )?),
                None => None,
            };
            if align.is_some() && reference.is_none() {
                return Err(fail(
                    ValidationCode::BadArgument,
                    "payload.ref",
                    "Align needs a ref to scroll to",
                ));
            }
            if reference.is_some() && (delta_x.is_some() || delta_y.is_some()) {
                return Err(fail(
                    ValidationCode::BadArgument,
                    "payload",
                    "Scroll must use either a ref or a delta, not both",
                ));
            }
            if reference.is_none() && delta_x.is_none() && delta_y.is_none() {
                return Err(fail(
                    ValidationCode::BadArgument,
                    "payload",
                    "Scroll needs a ref or a delta",
                ));
            }
            Ok(RequestPayload::Scroll {
                binding,
                observation_id,
                reference,
                align,
                delta_x,
                delta_y,
            })
        }
        "cancel" => {
            const ALLOWED: &[&str] = &["op", "requestId"];
            expect_keys(obj, ALLOWED, ALLOWED, "payload")?;
            Ok(RequestPayload::Cancel {
                request_id: expect_id(obj.get("requestId"), "payload.requestId")?,
            })
        }
        _ => Err(fail(
            ValidationCode::UnsupportedOp,
            "payload.op",
            "Unknown operation",
        )),
    }
}

fn expect_result_payload(value: Option<&Value>) -> NaviResult<ResultPayload> {
    let value =
        value.ok_or_else(|| fail(ValidationCode::BadEnvelope, "payload", "Missing field"))?;
    let obj = expect_record(value, "payload")?;
    let Some(op) = obj.get("op").and_then(Value::as_str) else {
        return Err(fail(
            ValidationCode::BadEnvelope,
            "payload.op",
            "Expected an operation",
        ));
    };
    match op {
        "observe" => {
            const ALLOWED: &[&str] = &["op", "observation"];
            expect_keys(obj, ALLOWED, ALLOWED, "payload")?;
            let observation = obj.get("observation").ok_or_else(|| {
                fail(
                    ValidationCode::BadEnvelope,
                    "payload.observation",
                    "Missing field",
                )
            })?;
            Ok(ResultPayload::Observe {
                observation: expect_observation(observation, "payload.observation")?,
            })
        }
        "focus" => {
            const ALLOWED: &[&str] = &["op", "outcome"];
            expect_keys(obj, ALLOWED, ALLOWED, "payload")?;
            Ok(ResultPayload::Focus {
                outcome: expect_enum(obj.get("outcome"), "payload.outcome", OUTCOMES)?,
            })
        }
        "scroll" => {
            const ALLOWED: &[&str] = &["op", "outcome", "scrollX", "scrollY"];
            const REQUIRED: &[&str] = &["op", "outcome"];
            expect_keys(obj, ALLOWED, REQUIRED, "payload")?;
            let scroll_x = match obj.get("scrollX") {
                Some(value) => Some(expect_int(
                    Some(value),
                    "payload.scrollX",
                    -1_000_000_000,
                    1_000_000_000,
                )?),
                None => None,
            };
            let scroll_y = match obj.get("scrollY") {
                Some(value) => Some(expect_int(
                    Some(value),
                    "payload.scrollY",
                    -1_000_000_000,
                    1_000_000_000,
                )?),
                None => None,
            };
            Ok(ResultPayload::Scroll {
                outcome: expect_enum(obj.get("outcome"), "payload.outcome", OUTCOMES)?,
                scroll_x,
                scroll_y,
            })
        }
        "cancel" => {
            const ALLOWED: &[&str] = &["op", "cancelled", "alreadyTerminal"];
            expect_keys(obj, ALLOWED, ALLOWED, "payload")?;
            Ok(ResultPayload::Cancel {
                cancelled: expect_bool(obj.get("cancelled"), "payload.cancelled")?,
                already_terminal: expect_bool(
                    obj.get("alreadyTerminal"),
                    "payload.alreadyTerminal",
                )?,
            })
        }
        _ => Err(fail(
            ValidationCode::UnsupportedOp,
            "payload.op",
            "Unknown operation",
        )),
    }
}

/// Validates one untrusted JSON value and returns the typed frame it describes.
pub fn validate_frame(value: &Value) -> NaviResult<ProtocolFrame> {
    let frame = expect_record(value, "")?;
    let (seq, frame_type) = expect_header(frame)?;
    // Budget first: a peer may not force deep validation of an oversized payload.
    if json_byte_len(value) > MAX_FRAME_BYTES {
        return Err(fail(
            ValidationCode::EnvelopeTooLarge,
            "",
            &format!("Frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    match frame_type.as_str() {
        "hello" => {
            const ALLOWED: &[&str] = &["v", "seq", "type", "peer", "capabilities"];
            expect_keys(frame, ALLOWED, ALLOWED, "")?;
            Ok(ProtocolFrame::Hello {
                v: NAVI_PROTOCOL_VERSION,
                seq,
                peer: expect_peer(frame.get("peer"), "peer")?,
                capabilities: expect_capability_list(frame.get("capabilities"), "capabilities")?,
            })
        }
        "hello.ack" => {
            const ALLOWED: &[&str] = &["v", "seq", "type", "connectionId", "peer", "capabilities"];
            expect_keys(frame, ALLOWED, ALLOWED, "")?;
            Ok(ProtocolFrame::HelloAck {
                v: NAVI_PROTOCOL_VERSION,
                seq,
                connection_id: expect_id(frame.get("connectionId"), "connectionId")?,
                peer: expect_peer(frame.get("peer"), "peer")?,
                capabilities: expect_capability_list(frame.get("capabilities"), "capabilities")?,
            })
        }
        "hello.error" => {
            const ALLOWED: &[&str] = &["v", "seq", "type", "error"];
            expect_keys(frame, ALLOWED, ALLOWED, "")?;
            Ok(ProtocolFrame::HelloError {
                v: NAVI_PROTOCOL_VERSION,
                seq,
                error: expect_error(frame.get("error"), "error")?,
            })
        }
        "request" => {
            const ALLOWED: &[&str] = &["v", "seq", "type", "connectionId", "requestId", "payload"];
            expect_keys(frame, ALLOWED, ALLOWED, "")?;
            Ok(ProtocolFrame::Request {
                v: NAVI_PROTOCOL_VERSION,
                seq,
                connection_id: expect_id(frame.get("connectionId"), "connectionId")?,
                request_id: expect_id(frame.get("requestId"), "requestId")?,
                payload: expect_request_payload(frame.get("payload"))?,
            })
        }
        "result" => {
            const ALLOWED: &[&str] = &[
                "v",
                "seq",
                "type",
                "connectionId",
                "requestId",
                "ok",
                "payload",
                "error",
            ];
            expect_keys(frame, ALLOWED, &[], "")?;
            let ok = expect_bool(frame.get("ok"), "ok")?;
            let required: &[&str] = if ok {
                &[
                    "v",
                    "seq",
                    "type",
                    "connectionId",
                    "requestId",
                    "ok",
                    "payload",
                ]
            } else {
                &[
                    "v",
                    "seq",
                    "type",
                    "connectionId",
                    "requestId",
                    "ok",
                    "error",
                ]
            };
            expect_keys(frame, required, required, "")?;
            let connection_id = expect_id(frame.get("connectionId"), "connectionId")?;
            let request_id = expect_id(frame.get("requestId"), "requestId")?;
            if ok {
                Ok(ProtocolFrame::ResultOk {
                    v: NAVI_PROTOCOL_VERSION,
                    seq,
                    connection_id,
                    request_id,
                    ok,
                    payload: expect_result_payload(frame.get("payload"))?,
                })
            } else {
                Ok(ProtocolFrame::ResultErr {
                    v: NAVI_PROTOCOL_VERSION,
                    seq,
                    connection_id,
                    request_id,
                    ok,
                    error: expect_error(frame.get("error"), "error")?,
                })
            }
        }
        "event" => {
            const ALLOWED: &[&str] = &["v", "seq", "type", "connectionId", "kind", "payload"];
            expect_keys(frame, ALLOWED, ALLOWED, "")?;
            let payload = expect_record(
                frame
                    .get("payload")
                    .ok_or_else(|| fail(ValidationCode::BadEnvelope, "payload", "Missing field"))?,
                "payload",
            )?;
            expect_keys(
                payload,
                &["grant", "reason"],
                &["grant", "reason"],
                "payload",
            )?;
            let grant = match payload.get("grant") {
                Some(Value::Null) | None => None,
                Some(value) => Some(expect_grant(value, "payload.grant")?),
            };
            Ok(ProtocolFrame::Event {
                v: NAVI_PROTOCOL_VERSION,
                seq,
                connection_id: expect_id(frame.get("connectionId"), "connectionId")?,
                kind: expect_enum(frame.get("kind"), "kind", EVENT_KINDS)?,
                payload: EventPayload {
                    grant,
                    reason: expect_enum(
                        payload.get("reason"),
                        "payload.reason",
                        GRANT_CHANGE_REASONS,
                    )?,
                },
            })
        }
        "bye" => {
            const ALLOWED: &[&str] = &["v", "seq", "type", "connectionId", "reason"];
            expect_keys(frame, ALLOWED, ALLOWED, "")?;
            Ok(ProtocolFrame::Bye {
                v: NAVI_PROTOCOL_VERSION,
                seq,
                connection_id: expect_id(frame.get("connectionId"), "connectionId")?,
                reason: expect_enum(frame.get("reason"), "reason", BYE_REASONS)?,
            })
        }
        _ => Err(fail(
            ValidationCode::BadEnvelope,
            "type",
            "Unknown frame type",
        )),
    }
}

/// Validates `frame` and serializes it to one JSON object string.
pub fn encode_frame(frame: impl Borrow<ProtocolFrame>) -> NaviResult<String> {
    let value = serde_json::to_value(frame.borrow())?;
    let validated = validate_frame(&value)?;
    let text = serde_json::to_string(&validated)?;
    if text.len() > MAX_FRAME_BYTES {
        return Err(fail(
            ValidationCode::EnvelopeTooLarge,
            "",
            &format!("Frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    Ok(text)
}

/// Parses one JSON object string into a validated frame.
pub fn decode_frame(text: impl AsRef<str>) -> NaviResult<ProtocolFrame> {
    let value: Value = serde_json::from_str(text.as_ref())
        .map_err(|_| fail(ValidationCode::BadJson, "", "Frame text is not valid JSON"))?;
    validate_frame(&value)
}

/// UTF-8 byte length of a frame as it would be sent, without serializing it twice.
pub fn frame_byte_length(frame: impl Borrow<ProtocolFrame>) -> NaviResult<usize> {
    Ok(encode_frame(frame)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding() -> Binding {
        Binding {
            grant_id: "grant-1".into(),
            tab_id: 7,
            document_id: "doc-1".into(),
            page_epoch: 1,
        }
    }

    fn peer() -> PeerInfo {
        PeerInfo {
            kind: PeerKind::Cli,
            version: "1.0.0".into(),
        }
    }

    fn hello(seq: u64) -> ProtocolFrame {
        ProtocolFrame::Hello {
            v: NAVI_PROTOCOL_VERSION,
            seq,
            peer: peer(),
            capabilities: vec![Capability::Observe],
        }
    }

    fn wire_error() -> WireError {
        WireError {
            code: WireErrorCode::BadArgument,
            message: "bad input".into(),
            retryable: false,
        }
    }

    fn request(request_id: &str, payload: RequestPayload) -> ProtocolFrame {
        ProtocolFrame::Request {
            v: NAVI_PROTOCOL_VERSION,
            seq: 2,
            connection_id: "conn-1".into(),
            request_id: request_id.into(),
            payload,
        }
    }

    fn observation() -> Observation {
        Observation {
            observation_id: "obs-1".into(),
            binding: binding(),
            captured_at: 1_700_000_000_000,
            title: Some("Example".into()),
            url: "https://example.com/page".into(),
            nodes: vec![DomNode {
                reference: "n1".into(),
                depth: 1,
                tag: "button".into(),
                role: Some("button".into()),
                name: Some("Open".into()),
                text: None,
                interactive: true,
                editable: false,
                offscreen: false,
                rect: Some(DomRect {
                    x: -1,
                    y: 2,
                    w: 30,
                    h: 12,
                }),
            }],
            visited: 1,
            truncated: false,
            omitted: vec![],
        }
    }

    #[test]
    fn capability_helpers_and_limits() {
        assert!(is_capability("observe"));
        assert!(is_capability("focus"));
        assert!(is_capability("scroll"));
        assert!(!is_capability("Observe"));
        assert!(!is_capability(""));
        assert!(!is_capability("navigate"));
        assert_eq!(MAX_FRAME_BYTES, 65_536);
        assert_eq!(MAX_OBSERVATION_BYTES, 16_384);
        assert_eq!(MAX_SEQUENCE, 9_007_199_254_740_991);
        assert_eq!(
            serde_json::to_string(&Capability::Focus).expect("serialize capability"),
            "\"focus\""
        );
    }

    #[test]
    fn hello_encode_decode_roundtrip() {
        let frame = hello(1);
        let text = encode_frame(&frame).expect("encode hello");
        let value: Value = serde_json::from_str(&text).expect("parse encoded text");
        let object = value.as_object().expect("encoded frame is an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["capabilities", "peer", "seq", "type", "v"]);
        assert_eq!(object.get("v"), Some(&json!(1)));
        assert_eq!(decode_frame(&text).expect("decode hello"), frame);
    }

    #[test]
    fn request_scroll_validation() {
        let by_ref = request(
            "req-1",
            RequestPayload::Scroll {
                binding: binding(),
                observation_id: "obs-1".into(),
                reference: Some("n1".into()),
                align: Some(ScrollAlign::Center),
                delta_x: None,
                delta_y: None,
            },
        );
        let decoded =
            decode_frame(encode_frame(&by_ref).expect("encode scroll")).expect("decode scroll");
        assert_eq!(decoded, by_ref);

        let by_delta = request(
            "req-2",
            RequestPayload::Scroll {
                binding: binding(),
                observation_id: "obs-1".into(),
                reference: None,
                align: None,
                delta_x: Some(-MAX_SCROLL_DELTA),
                delta_y: None,
            },
        );
        assert_eq!(
            decode_frame(encode_frame(&by_delta).expect("encode delta")).expect("decode delta"),
            by_delta
        );

        let scroll_json = |extra: Value| -> Value {
            let mut payload = json!({
                "op": "scroll",
                "binding": {
                    "grantId": "grant-1",
                    "tabId": 7,
                    "documentId": "doc-1",
                    "pageEpoch": 1
                },
                "observationId": "obs-1"
            });
            let payload_object = payload.as_object_mut().expect("payload object");
            for (key, value) in extra.as_object().expect("extra object") {
                payload_object.insert(key.clone(), value.clone());
            }
            json!({
                "v": 1,
                "seq": 1,
                "type": "request",
                "connectionId": "conn-1",
                "requestId": "req-1",
                "payload": payload
            })
        };
        assert!(validate_frame(&scroll_json(json!({"align": "start"}))).is_err());
        assert!(validate_frame(&scroll_json(json!({"ref": "n1", "deltaY": 5}))).is_err());
        assert!(validate_frame(&scroll_json(json!({}))).is_err());
        assert!(validate_frame(&scroll_json(json!({"deltaX": 1001}))).is_err());
    }

    #[test]
    fn rejects_bad_headers_and_unknown_fields() {
        let valid = json!({"v": 1, "seq": 1, "type": "hello", "peer": {"kind": "cli", "version": "1.0.0"}, "capabilities": ["observe"]});
        assert!(validate_frame(&valid).is_ok());
        assert!(validate_frame(&json!({"v": 2, "seq": 1, "type": "hello", "peer": {"kind": "cli", "version": "1.0.0"}, "capabilities": ["observe"]})).is_err());
        assert!(validate_frame(&json!({"v": 1, "seq": 0, "type": "hello", "peer": {"kind": "cli", "version": "1.0.0"}, "capabilities": ["observe"]})).is_err());
        assert!(validate_frame(&json!({"v": 1, "seq": 1.5, "type": "hello", "peer": {"kind": "cli", "version": "1.0.0"}, "capabilities": ["observe"]})).is_err());
        assert!(validate_frame(&json!({"v": 1, "seq": 1, "type": "unknown", "peer": {"kind": "cli", "version": "1.0.0"}, "capabilities": ["observe"]})).is_err());
        let mut extra = valid.clone();
        extra
            .as_object_mut()
            .expect("object")
            .insert("extra".into(), json!(1));
        assert!(validate_frame(&extra).is_err());
        let mut missing = valid;
        missing.as_object_mut().expect("object").remove("peer");
        assert!(validate_frame(&missing).is_err());
        assert!(decode_frame("not json").is_err());
    }

    #[test]
    fn observation_bounds() {
        let result = ProtocolFrame::ResultOk {
            v: NAVI_PROTOCOL_VERSION,
            seq: 3,
            connection_id: "conn-1".into(),
            request_id: "req-1".into(),
            ok: true,
            payload: ResultPayload::Observe {
                observation: observation(),
            },
        };
        assert_eq!(
            decode_frame(encode_frame(&result).expect("encode observation"))
                .expect("decode observation"),
            result
        );

        let observation_json = |changes: Value| -> Value {
            let mut observation_value = json!({
                "observationId": "obs-1",
                "binding": {"grantId": "grant-1", "tabId": 7, "documentId": "doc-1", "pageEpoch": 1},
                "capturedAt": 1,
                "url": "https://example.com/page",
                "nodes": [],
                "visited": 0,
                "truncated": false,
                "omitted": []
            });
            for (key, value) in changes.as_object().expect("changes object") {
                observation_value
                    .as_object_mut()
                    .expect("observation object")
                    .insert(key.clone(), value.clone());
            }
            json!({
                "v": 1,
                "seq": 1,
                "type": "result",
                "connectionId": "conn-1",
                "requestId": "req-1",
                "ok": true,
                "payload": {"op": "observe", "observation": observation_value}
            })
        };

        assert!(
            validate_frame(&observation_json(
                json!({"url": "https://user@example.com/page"})
            ))
            .is_err()
        );
        assert!(
            validate_frame(&observation_json(
                json!({"url": "https://example.com/?q=1"})
            ))
            .is_err()
        );
        assert!(
            validate_frame(&observation_json(
                json!({"url": "https://example.com:443/page"})
            ))
            .is_err()
        );
        assert!(validate_frame(&observation_json(json!({"title": "line\nbreak"}))).is_err());
        assert!(validate_frame(&observation_json(json!({"nodes": [], "visited": -1}))).is_err());

        let mut too_many = Vec::new();
        for index in 0..=MAX_NODES {
            too_many.push(json!({
                "ref": format!("n{index}"),
                "depth": 0,
                "tag": "div",
                "interactive": false,
                "editable": false,
                "offscreen": false
            }));
        }
        assert!(
            validate_frame(&observation_json(
                json!({"nodes": too_many, "visited": 101})
            ))
            .is_err()
        );

        let mut large = Vec::new();
        for index in 0..MAX_NODES {
            large.push(json!({
                "ref": format!("n{index}"),
                "depth": 0,
                "tag": "div",
                "name": "x".repeat(MAX_TEXT_CHARS),
                "interactive": false,
                "editable": false,
                "offscreen": false
            }));
        }
        assert!(
            validate_frame(&observation_json(json!({"nodes": large, "visited": 100}))).is_err()
        );
    }

    #[test]
    fn result_and_error_frames() {
        let ok = ProtocolFrame::ResultOk {
            v: NAVI_PROTOCOL_VERSION,
            seq: 4,
            connection_id: "conn-1".into(),
            request_id: "req-1".into(),
            ok: true,
            payload: ResultPayload::Focus {
                outcome: EffectOutcome::Unknown,
            },
        };
        assert_eq!(
            decode_frame(encode_frame(&ok).expect("encode ok")).expect("decode ok"),
            ok
        );

        let failed = ProtocolFrame::ResultErr {
            v: NAVI_PROTOCOL_VERSION,
            seq: 5,
            connection_id: "conn-1".into(),
            request_id: "req-1".into(),
            ok: false,
            error: wire_error(),
        };
        assert_eq!(
            decode_frame(encode_frame(&failed).expect("encode failed")).expect("decode failed"),
            failed
        );

        let hello_error = ProtocolFrame::HelloError {
            v: NAVI_PROTOCOL_VERSION,
            seq: 6,
            error: wire_error(),
        };
        assert_eq!(
            decode_frame(encode_frame(&hello_error).expect("encode error")).expect("decode error"),
            hello_error
        );

        let transport_local = json!({
            "v": 1,
            "seq": 6,
            "type": "hello.error",
            "error": {"code": "BAD_JSON", "message": "bad", "retryable": false}
        });
        assert!(validate_frame(&transport_local).is_err());
    }

    #[test]
    fn event_and_bye_roundtrip() {
        let event = ProtocolFrame::Event {
            v: NAVI_PROTOCOL_VERSION,
            seq: 7,
            connection_id: "conn-1".into(),
            kind: EventKind::BindingChanged,
            payload: EventPayload {
                grant: None,
                reason: GrantChangeReason::Detached,
            },
        };
        assert_eq!(
            decode_frame(encode_frame(&event).expect("encode event")).expect("decode event"),
            event
        );

        let with_grant = ProtocolFrame::Event {
            v: NAVI_PROTOCOL_VERSION,
            seq: 8,
            connection_id: "conn-1".into(),
            kind: EventKind::BindingChanged,
            payload: EventPayload {
                grant: Some(Grant {
                    grant_id: "grant-1".into(),
                    binding: binding(),
                    capabilities: vec![Capability::Observe, Capability::Focus],
                }),
                reason: GrantChangeReason::Attached,
            },
        };
        assert_eq!(
            decode_frame(encode_frame(&with_grant).expect("encode grant event"))
                .expect("decode grant event"),
            with_grant
        );

        let bye = ProtocolFrame::Bye {
            v: NAVI_PROTOCOL_VERSION,
            seq: 9,
            connection_id: "conn-1".into(),
            reason: ByeReason::Idle,
        };
        assert_eq!(
            decode_frame(encode_frame(&bye).expect("encode bye")).expect("decode bye"),
            bye
        );
    }

    #[test]
    fn frame_byte_length_and_envelope_budget() {
        let frame = hello(1);
        let text = encode_frame(&frame).expect("encode hello");
        assert_eq!(frame_byte_length(&frame).expect("byte length"), text.len());

        let mut oversized = json!({
            "v": 1,
            "seq": 1,
            "type": "hello",
            "peer": {"kind": "cli", "version": "1.0.0"},
            "capabilities": ["observe"],
            "padding": "x".repeat(MAX_FRAME_BYTES)
        });
        let oversized_text = oversized.to_string();
        assert!(decode_frame(&oversized_text).is_err());
        oversized.as_object_mut().expect("object").remove("padding");
        assert!(validate_frame(&oversized).is_ok());
    }
}
