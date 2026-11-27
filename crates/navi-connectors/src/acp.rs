//! ACP (Agent Client Protocol) v1 wire helpers ported from `apps/navi/workers/acp_client.py`.
//!
//! The module covers the four pure concerns of the Python worker: JSON-RPC 2.0
//! framing and line parsing, the `initialize` / `session/prompt` request builders,
//! normalization of `session/update` notifications into Navi's record stream, and
//! the permission approval policy used when replying to `session/request_permission`.
//! Process spawning and I/O stay outside this module; everything here is pure and
//! testable without an ACP harness.

use std::collections::HashMap;
use std::path::Path;

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};

/// JSON-RPC version marker carried by every ACP message.
pub const JSON_RPC_VERSION: &str = "2.0";
/// ACP protocol version advertised by the Navi client.
pub const ACP_PROTOCOL_VERSION: u64 = 1;
/// Client name advertised during `initialize`.
pub const CLIENT_NAME: &str = "navi";
/// Client version advertised during `initialize`.
pub const CLIENT_VERSION: &str = "0.1.0";

/// JSON-RPC error code for an agent method Navi does not implement.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code for a request with rejected parameters.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error code for an unexpected harness-side failure.
pub const INTERNAL_ERROR: i64 = -32000;

/// A JSON-RPC request identifier: an integer or a string.
///
/// Booleans, floats, null, arrays and objects are deliberately not accepted:
/// the Python worker discards inbound messages whose `id` has one of those types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id, normally Navi's monotonically increasing counter.
    Number(Number),
    /// String id chosen by the agent.
    String(String),
}

impl RequestId {
    /// Validates an untrusted JSON value as a JSON-RPC request id.
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) if number.is_i64() || number.is_u64() => {
                Some(Self::Number(number.clone()))
            }
            Value::String(text) => Some(Self::String(text.clone())),
            _ => None,
        }
    }
}

impl From<u64> for RequestId {
    fn from(value: u64) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<i32> for RequestId {
    fn from(value: i32) -> Self {
        Self::Number(Number::from(value))
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<String> for RequestId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

/// An outbound JSON-RPC 2.0 request. A request without an id is a notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpJsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Present for requests that expect a reply, absent for notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    /// ACP method name, for example `session/prompt`.
    pub method: String,
    /// Method parameters; omitted parameters are represented by `{}`.
    #[serde(default)]
    pub params: Value,
}

impl AcpJsonRpcRequest {
    /// Builds a request with an id.
    pub fn new(id: impl Into<RequestId>, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: Some(id.into()),
            method: method.into(),
            params,
        }
    }

    /// Builds a notification, which has no id and expects no reply.
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: None,
            method: method.into(),
            params,
        }
    }

    /// Reports whether this message is a notification.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// Serializes the request as one newline-terminated ACP frame.
    pub fn to_line(&self) -> NaviResult<String> {
        encode_acp_line(self)
    }
}

/// A JSON-RPC 2.0 error object sent by either side of an ACP connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpJsonRpcError {
    /// Numeric JSON-RPC error code.
    pub code: i64,
    /// Human-readable failure description.
    pub message: String,
    /// Optional structured details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl AcpJsonRpcError {
    /// Builds an error object with optional structured details.
    pub fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    /// Error returned for a method Navi does not implement.
    pub fn unsupported_method(method: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            format!("Unsupported method: {method}"),
            None,
        )
    }

    /// Error returned for a parameter that fails the workspace boundary check.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message, None)
    }

    /// Error returned for an unexpected failure while handling a request.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message, None)
    }
}

/// An inbound JSON-RPC 2.0 reply to one of Navi's requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpJsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Id of the request being answered.
    pub id: RequestId,
    /// Successful payload, absent when `error` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure object, absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AcpJsonRpcError>,
}

impl AcpJsonRpcResponse {
    /// Builds a successful reply.
    pub fn success(id: impl Into<RequestId>, result: impl Into<Value>) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: id.into(),
            result: Some(result.into()),
            error: None,
        }
    }

    /// Builds a failed reply.
    pub fn failure(id: impl Into<RequestId>, error: AcpJsonRpcError) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: id.into(),
            result: None,
            error: Some(error),
        }
    }

    /// Reports whether the reply carries an error.
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Serializes the reply as one newline-terminated ACP frame.
    pub fn to_line(&self) -> NaviResult<String> {
        encode_acp_line(self)
    }

    /// Converts the reply into the value a waiting caller would receive.
    ///
    /// An error reply becomes a [`NaviError::Protocol`] carrying the agent's message,
    /// mirroring the exception raised by `AcpClient.request`.
    pub fn into_result(self) -> NaviResult<Value> {
        match self.error {
            Some(error) => Err(NaviError::Protocol(format!(
                "ACP request failed: {}",
                error.message
            ))),
            None => Ok(self.result.unwrap_or(Value::Null)),
        }
    }
}

/// Any valid ACP message that can arrive on the agent's stdout.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpJsonRpcMessage {
    /// Agent-initiated request that expects a reply from Navi.
    Request(AcpJsonRpcRequest),
    /// Agent-initiated notification that expects no reply.
    Notification(AcpJsonRpcRequest),
    /// Reply to a request previously sent by Navi.
    Response(AcpJsonRpcResponse),
}

impl AcpJsonRpcMessage {
    /// Serializes the message as one newline-terminated ACP frame.
    pub fn to_line(&self) -> NaviResult<String> {
        match self {
            Self::Request(request) | Self::Notification(request) => encode_acp_line(request),
            Self::Response(response) => encode_acp_line(response),
        }
    }
}

/// Encodes any serializable ACP payload as one NDJSON frame.
pub fn encode_acp_line<T: Serialize>(payload: &T) -> NaviResult<String> {
    let mut line = serde_json::to_string(payload)?;
    line.push('\n');
    Ok(line)
}

/// Parses one NDJSON line from an ACP agent into a typed message.
///
/// The Python worker skips blank lines, non-JSON lines, non-object messages and
/// messages with a malformed `id`. This function reports those cases as a
/// [`NaviError::Protocol`] so callers can log them and keep reading.
pub fn parse_acp_line(line: &str) -> NaviResult<AcpJsonRpcMessage> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(NaviError::Protocol("empty ACP line".to_owned()));
    }

    let value: Value = serde_json::from_str(trimmed)
        .map_err(|err| NaviError::Protocol(format!("invalid ACP JSON line: {err}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| NaviError::Protocol("ACP message must be a JSON object".to_owned()))?;

    if let Some(raw_id) = object.get("id")
        && RequestId::from_value(raw_id).is_none()
    {
        return Err(NaviError::Protocol(format!(
            "invalid ACP message id: {raw_id}"
        )));
    }

    let jsonrpc = object
        .get("jsonrpc")
        .and_then(Value::as_str)
        .unwrap_or(JSON_RPC_VERSION)
        .to_owned();

    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result || has_error {
        if has_result && has_error {
            return Err(NaviError::Protocol(
                "ACP reply cannot contain both result and error".to_owned(),
            ));
        }
        let raw_id = object
            .get("id")
            .ok_or_else(|| NaviError::Protocol("ACP reply is missing its request id".to_owned()))?;
        let id = RequestId::from_value(raw_id)
            .ok_or_else(|| NaviError::Protocol(format!("invalid ACP reply id: {raw_id}")))?;
        let error = object
            .get("error")
            .filter(|value| !value.is_null())
            .map(parse_error);
        return Ok(AcpJsonRpcMessage::Response(AcpJsonRpcResponse {
            jsonrpc,
            id,
            result: object.get("result").cloned(),
            error,
        }));
    }

    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| NaviError::Protocol("ACP message is missing a string method".to_owned()))?;
    let params = match object.get("params") {
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => {
            return Err(NaviError::Protocol(
                "ACP message params must be a JSON object".to_owned(),
            ));
        }
    };
    let id = object.get("id").and_then(RequestId::from_value);
    let request = AcpJsonRpcRequest {
        jsonrpc,
        id,
        method: method.to_owned(),
        params,
    };
    if request.is_notification() {
        Ok(AcpJsonRpcMessage::Notification(request))
    } else {
        Ok(AcpJsonRpcMessage::Request(request))
    }
}

/// Builds the `initialize` request advertising Navi's capabilities.
pub fn build_initialize_request(id: impl Into<RequestId>) -> AcpJsonRpcRequest {
    AcpJsonRpcRequest::new(
        id,
        "initialize",
        json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": false,
            },
            "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
        }),
    )
}

/// Builds the `session/new` request for the given working directory.
pub fn build_session_new_request(
    id: impl Into<RequestId>,
    cwd: impl AsRef<Path>,
) -> AcpJsonRpcRequest {
    let cwd = cwd.as_ref().to_string_lossy().into_owned();
    AcpJsonRpcRequest::new(id, "session/new", json!({"cwd": cwd, "mcpServers": []}))
}

/// Builds the `session/prompt` request with a single text prompt block.
pub fn build_session_prompt_request(
    id: impl Into<RequestId>,
    session_id: &str,
    text: &str,
) -> AcpJsonRpcRequest {
    AcpJsonRpcRequest::new(
        id,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": text}],
        }),
    )
}

/// Builds the `session/cancel` notification for an active session.
pub fn build_cancel_notification(session_id: &str) -> AcpJsonRpcRequest {
    AcpJsonRpcRequest::notification("session/cancel", json!({"sessionId": session_id}))
}

/// Reads `sessionId` from a `session/new` result, ignoring non-string values.
pub fn session_id_from_result(result: Option<&Value>) -> Option<String> {
    result
        .and_then(|value| value.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Reads `stopReason` from a `session/prompt` result, defaulting to `end_turn`.
pub fn stop_reason_from_result(result: Option<&Value>) -> String {
    result
        .and_then(|value| value.get("stopReason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("end_turn")
        .to_owned()
}

/// A normalized record in Navi's ACP output stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpUpdateEvent {
    /// Assistant message text (`agent_message_chunk`).
    Delta {
        /// Text chunk to append to the current answer.
        text: String,
    },
    /// Assistant reasoning text (`agent_thought_chunk`).
    Reasoning {
        /// Text chunk to append to the current reasoning trace.
        text: String,
    },
    /// A tool call that is still running.
    ToolStatus {
        /// Stable identifier of the tool call.
        id: String,
        /// Last advertised title, if any.
        name: Option<String>,
        /// Raw ACP status, defaulting to `running`.
        status: String,
        /// Last advertised raw input, if any.
        input: Option<Value>,
    },
    /// A completed, failed or errored tool call.
    Tool {
        /// Stable identifier of the tool call.
        id: String,
        /// Last advertised title, if any.
        name: Option<String>,
        /// Final ACP status.
        status: String,
        /// Last advertised raw input, if any.
        input: Option<Value>,
        /// Raw output, or text extracted from a content list.
        output: Option<Value>,
        /// Always `true`; present because the emitted record uses that key.
        #[serde(rename = "final")]
        is_final: bool,
    },
}

/// Names and inputs remembered across updates of the same tool call.
#[derive(Debug, Clone, Default, PartialEq)]
struct AcpToolState {
    name: Option<String>,
    input: Option<Value>,
}

/// Stateful normalizer for ACP `session/update` notifications.
///
/// Tool titles and raw inputs can arrive in separate updates, so the normalizer
/// keeps the last known value for each `toolCallId` until the tool finishes.
#[derive(Debug, Clone, Default)]
pub struct AcpUpdateNormalizer {
    tools: HashMap<String, AcpToolState>,
}

impl AcpUpdateNormalizer {
    /// Creates an empty normalizer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalizes a method/params pair, ignoring methods other than `session/update`.
    pub fn push_notification(&mut self, method: &str, params: &Value) -> Vec<AcpUpdateEvent> {
        if method != "session/update" {
            return Vec::new();
        }
        let Some(update) = params.get("update").filter(|value| value.is_object()) else {
            return Vec::new();
        };
        self.push_update(update)
    }

    /// Normalizes one `update` object into zero or more output records.
    pub fn push_update(&mut self, update: &Value) -> Vec<AcpUpdateEvent> {
        let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return Vec::new();
        };
        match kind {
            "agent_message_chunk" => text_chunk(update)
                .map(|text| vec![AcpUpdateEvent::Delta { text }])
                .unwrap_or_default(),
            "agent_thought_chunk" => text_chunk(update)
                .map(|text| vec![AcpUpdateEvent::Reasoning { text }])
                .unwrap_or_default(),
            "tool_call" | "tool_call_update" => self.tool_update(update),
            _ => Vec::new(),
        }
    }

    /// Number of tool calls currently being tracked.
    pub fn tracked_tools(&self) -> usize {
        self.tools.len()
    }

    fn tool_update(&mut self, update: &Value) -> Vec<AcpUpdateEvent> {
        let Some(id) = scalar_string(update.get("toolCallId")) else {
            return Vec::new();
        };
        if id.is_empty() {
            return Vec::new();
        }

        let state = self.tools.entry(id.clone()).or_default();
        if let Some(title) = nonempty_string(update.get("title")) {
            state.name = Some(title);
        }
        if let Some(input) = update.get("rawInput").filter(|value| !value.is_null()) {
            state.input = Some(input.clone());
        }
        let status = nonempty_string(update.get("status")).unwrap_or_else(|| "running".to_owned());

        if matches!(status.as_str(), "completed" | "failed" | "error") {
            let output = update
                .get("rawOutput")
                .filter(|value| !value.is_null())
                .cloned()
                .or_else(|| content_list_text(update.get("content")));
            vec![AcpUpdateEvent::Tool {
                id,
                name: state.name.clone(),
                status,
                input: state.input.clone(),
                output,
                is_final: true,
            }]
        } else {
            vec![AcpUpdateEvent::ToolStatus {
                id,
                name: state.name.clone(),
                status,
                input: state.input.clone(),
            }]
        }
    }
}

/// Normalizes a single update object without retaining tool-call state.
pub fn normalize_update_event(update: &Value) -> Vec<AcpUpdateEvent> {
    AcpUpdateNormalizer::new().push_update(update)
}

/// Normalizes a `session/update` notification without retaining tool-call state.
pub fn normalize_session_notification(method: &str, params: &Value) -> Vec<AcpUpdateEvent> {
    AcpUpdateNormalizer::new().push_notification(method, params)
}

/// A user's answer to an ACP `session/request_permission` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPermissionDecision {
    /// Approve this once (`allow_once`).
    Allow,
    /// Approve this and future matching calls (`allow_always`).
    Always,
    /// Reject this call (`reject_once`).
    Deny,
    /// No decision, or an unrecognized reply.
    Cancel,
}

impl AcpPermissionDecision {
    /// Returns the wire spelling of the decision.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Always => "always",
            Self::Deny => "deny",
            Self::Cancel => "cancel",
        }
    }

    /// Returns the ACP option kind this decision selects, if any.
    pub fn option_kind(self) -> Option<&'static str> {
        match self {
            Self::Allow => Some("allow_once"),
            Self::Always => Some("allow_always"),
            Self::Deny => Some("reject_once"),
            Self::Cancel => None,
        }
    }

    /// Reports whether the decision grants permission.
    pub fn is_approval(self) -> bool {
        matches!(self, Self::Allow | Self::Always)
    }
}

impl From<&str> for AcpPermissionDecision {
    fn from(value: &str) -> Self {
        match value {
            "allow" => Self::Allow,
            "always" => Self::Always,
            "deny" => Self::Deny,
            _ => Self::Cancel,
        }
    }
}

impl From<String> for AcpPermissionDecision {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

/// One option offered by an ACP permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpPermissionOption {
    /// Id echoed back in the permission reply.
    #[serde(rename = "optionId")]
    pub option_id: String,
    /// ACP kind such as `allow_once`, `allow_always` or `reject_once`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Human-readable label shown in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Extracts valid permission options from a `session/request_permission` params object.
///
/// As in the Python worker, options without a non-empty string `optionId` are dropped.
pub fn parse_permission_options(params: &Value) -> Vec<AcpPermissionOption> {
    params
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let option_id = value.get("optionId")?.as_str()?;
            if option_id.is_empty() {
                return None;
            }
            Some(AcpPermissionOption {
                option_id: option_id.to_owned(),
                kind: value.get("kind").and_then(Value::as_str).map(str::to_owned),
                name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            })
        })
        .collect()
}

/// Decisions shown to the user for a permission request.
///
/// Rejection is always offered, even when the agent sends no `reject_once` option;
/// in that case the eventual reply is `cancelled` rather than `selected`.
pub fn permission_choices(options: &[AcpPermissionOption]) -> Vec<AcpPermissionDecision> {
    [
        AcpPermissionDecision::Allow,
        AcpPermissionDecision::Always,
        AcpPermissionDecision::Deny,
    ]
    .into_iter()
    .filter(|decision| {
        *decision == AcpPermissionDecision::Deny
            || options
                .iter()
                .any(|option| option.kind.as_deref() == decision.option_kind())
    })
    .collect()
}

/// The outcome encoded in a permission reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpPermissionOutcome {
    /// The user selected the option with this id.
    Selected(String),
    /// No offered option matched, or the request was cancelled/timed out.
    Cancelled,
}

impl AcpPermissionOutcome {
    /// Builds the `result` payload sent back with the permission reply.
    pub fn to_result_value(&self) -> Value {
        match self {
            Self::Selected(option_id) => json!({"outcome": "selected", "optionId": option_id}),
            Self::Cancelled => json!({"outcome": "cancelled"}),
        }
    }
}

/// Result of applying the permission approval policy to a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpPermissionEvaluation {
    /// Decision that takes effect after validating it against the offered choices.
    pub decision: AcpPermissionDecision,
    /// Whether the reply window should be closed by this answer.
    pub accepted: bool,
    /// Outcome to send to the agent.
    pub outcome: AcpPermissionOutcome,
}

impl AcpPermissionEvaluation {
    /// Builds the `result` payload for the permission reply.
    pub fn reply_result(&self) -> Value {
        self.outcome.to_result_value()
    }
}

/// Applies Navi's permission approval policy to a user decision.
///
/// An approval that was not offered is downgraded to `cancel`; rejection is always
/// accepted. The selected option is the first offered option whose kind matches
/// the effective decision, mirroring `reply_permission` and `_handle_agent_request`.
pub fn evaluate_permission_policy(
    decision: impl Into<AcpPermissionDecision>,
    options: &[AcpPermissionOption],
) -> AcpPermissionEvaluation {
    let decision = decision.into();
    let choices = permission_choices(options);
    let accepted = !decision.is_approval() || choices.contains(&decision);
    let effective = if accepted {
        decision
    } else {
        AcpPermissionDecision::Cancel
    };

    let outcome = effective
        .option_kind()
        .and_then(|kind| {
            options
                .iter()
                .find(|option| option.kind.as_deref() == Some(kind))
        })
        .map(|option| AcpPermissionOutcome::Selected(option.option_id.clone()))
        .unwrap_or(AcpPermissionOutcome::Cancelled);

    AcpPermissionEvaluation {
        decision: effective,
        accepted,
        outcome,
    }
}

/// Builds the JSON-RPC reply for a `session/request_permission` request.
pub fn build_permission_reply(
    id: impl Into<RequestId>,
    decision: impl Into<AcpPermissionDecision>,
    options: &[AcpPermissionOption],
) -> AcpJsonRpcResponse {
    let evaluation = evaluate_permission_policy(decision, options);
    AcpJsonRpcResponse::success(id, evaluation.reply_result())
}

/// Returns the label shown for a tool call in a permission request.
pub fn permission_tool_label(tool_call: Option<&Value>) -> String {
    tool_call
        .and_then(|value| {
            nonempty_string(value.get("title")).or_else(|| nonempty_string(value.get("kind")))
        })
        .unwrap_or_else(|| "tool".to_owned())
}

/// Parses an error object (or a bare string, as some agents send) from the wire.
fn parse_error(value: &Value) -> AcpJsonRpcError {
    if value.is_object() {
        AcpJsonRpcError {
            code: value
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(INTERNAL_ERROR),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown ACP error")
                .to_owned(),
            data: value.get("data").cloned().filter(|data| !data.is_null()),
        }
    } else {
        AcpJsonRpcError::internal(value_to_text(value))
    }
}

/// Reads a non-empty string from `content.text` of a chunk update.
fn text_chunk(update: &Value) -> Option<String> {
    nonempty_string(
        update
            .get("content")
            .and_then(|content| content.get("text")),
    )
}

/// Reads a non-empty string, rejecting numbers and other JSON kinds.
fn nonempty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Converts a scalar JSON value to the string form used by the Python worker.
fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(true) => Some("True".to_owned()),
        Value::Bool(false) | Value::Null => None,
        _ => None,
    }
}

/// Joins the text blocks of a final tool update's `content` array.
fn content_list_text(value: Option<&Value>) -> Option<Value> {
    let items = value.and_then(Value::as_array)?;
    let mut parts = Vec::new();
    for item in items {
        let Some(inner) = item.get("content").filter(|content| content.is_object()) else {
            continue;
        };
        parts.push(inner.get("text").map(value_to_text).unwrap_or_default());
    }
    Some(Value::String(parts.join("\n").trim().to_owned()))
}

/// Renders a JSON value the way Python's `str()` would for tool content.
fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_request_carries_protocol_version_and_capabilities() {
        let request = build_initialize_request(1);
        assert_eq!(request.jsonrpc, JSON_RPC_VERSION);
        assert_eq!(request.method, "initialize");
        assert_eq!(request.id, Some(RequestId::from(1)));
        assert_eq!(
            request.params,
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": true, "writeTextFile": true},
                    "terminal": false,
                },
                "clientInfo": {"name": "navi", "version": "0.1.0"},
            })
        );
        let line = request.to_line().expect("serialize initialize request");
        assert!(line.ends_with('\n'));
        let parsed = parse_acp_line(&line).expect("parse initialize request");
        assert_eq!(parsed, AcpJsonRpcMessage::Request(request));
    }

    #[test]
    fn session_builders_produce_expected_wire_shapes() {
        let new_session = build_session_new_request(2, "/work/navi");
        assert_eq!(new_session.method, "session/new");
        assert_eq!(
            new_session.params,
            json!({"cwd": "/work/navi", "mcpServers": []})
        );

        let prompt = build_session_prompt_request(3, "sess-42", "hello");
        assert_eq!(prompt.method, "session/prompt");
        assert_eq!(
            prompt.params,
            json!({
                "sessionId": "sess-42",
                "prompt": [{"type": "text", "text": "hello"}],
            })
        );

        let cancel = build_cancel_notification("sess-42");
        assert!(cancel.is_notification());
        assert_eq!(cancel.method, "session/cancel");
        assert_eq!(cancel.params, json!({"sessionId": "sess-42"}));
        assert!(!cancel.to_line().expect("serialize").contains("\"id\""));
    }

    #[test]
    fn parse_acp_line_classifies_requests_notifications_and_replies() {
        let request = parse_acp_line(
            r#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{"path":"a.txt"}}"#,
        )
        .expect("request");
        assert!(matches!(request, AcpJsonRpcMessage::Request(_)));

        let notification =
            parse_acp_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{}}}"#)
                .expect("notification");
        assert!(matches!(notification, AcpJsonRpcMessage::Notification(_)));

        let response =
            parse_acp_line(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#).expect("response");
        let AcpJsonRpcMessage::Response(response) = response else {
            panic!("expected response");
        };
        assert_eq!(response.id, RequestId::from(7));
        assert_eq!(response.into_result().expect("result"), json!({"ok": true}));

        let failure = parse_acp_line(
            r#"{"jsonrpc":"2.0","id":"x","error":{"code":-32601,"message":"Unsupported method: nope"}}"#,
        )
        .expect("error response");
        let AcpJsonRpcMessage::Response(failure) = failure else {
            panic!("expected response");
        };
        assert!(failure.is_error());
        let err = failure.into_result().expect_err("error reply");
        assert!(matches!(err, NaviError::Protocol(_)));
        assert!(err.to_string().contains("Unsupported method: nope"));
    }

    #[test]
    fn parse_acp_line_rejects_malformed_and_untrusted_frames() {
        assert!(parse_acp_line("   ").is_err());
        assert!(parse_acp_line("not json").is_err());
        assert!(parse_acp_line("[1, 2, 3]").is_err());
        assert!(parse_acp_line(r#"{"jsonrpc":"2.0","params":{}}"#).is_err());
        assert!(parse_acp_line(r#"{"jsonrpc":"2.0","method":"m","params":[]}"#).is_err());
        assert!(parse_acp_line(r#"{"jsonrpc":"2.0","id":true,"method":"m"}"#).is_err());
        assert!(parse_acp_line(r#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#).is_err());
        assert!(parse_acp_line(r#"{"jsonrpc":"2.0","id":null,"method":"m"}"#).is_err());
        assert!(parse_acp_line(r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#).is_err());

        let string_id = parse_acp_line(r#"{"jsonrpc":"2.0","id":"abc","method":"m"}"#)
            .expect("string ids are valid");
        assert!(matches!(string_id, AcpJsonRpcMessage::Request(_)));
    }

    #[test]
    fn message_and_thought_chunks_are_normalized() {
        let mut normalizer = AcpUpdateNormalizer::new();
        let events = normalizer.push_notification(
            "session/update",
            &json!({
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hello "},
                }
            }),
        );
        assert_eq!(
            events,
            vec![AcpUpdateEvent::Delta {
                text: "hello ".into()
            }]
        );

        let events = normalizer.push_update(&json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"text": "thinking"},
        }));
        assert_eq!(
            events,
            vec![AcpUpdateEvent::Reasoning {
                text: "thinking".into()
            }]
        );

        assert!(
            normalizer
                .push_update(
                    &json!({"sessionUpdate": "agent_message_chunk", "content": {"text": ""}})
                )
                .is_empty()
        );
        assert!(
            normalizer
                .push_update(&json!({"sessionUpdate": "agent_message_chunk"}))
                .is_empty()
        );
        assert!(
            normalizer
                .push_update(&json!({"sessionUpdate": "unknown_kind"}))
                .is_empty()
        );
        assert!(
            normalizer
                .push_notification("session/request_permission", &json!({}))
                .is_empty()
        );
    }

    #[test]
    fn tool_updates_accumulate_state_and_emit_final_record() {
        let mut normalizer = AcpUpdateNormalizer::new();
        let events = normalizer.push_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tool-1",
            "title": "Read file",
            "rawInput": {"path": "main.rs"},
        }));
        assert_eq!(
            events,
            vec![AcpUpdateEvent::ToolStatus {
                id: "tool-1".into(),
                name: Some("Read file".into()),
                status: "running".into(),
                input: Some(json!({"path": "main.rs"})),
            }]
        );
        assert_eq!(normalizer.tracked_tools(), 1);

        let events = normalizer.push_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-1",
            "status": "completed",
            "content": [
                {"content": {"text": " first "}},
                {"content": {"text": "second "}},
            ],
        }));
        assert_eq!(
            events,
            vec![AcpUpdateEvent::Tool {
                id: "tool-1".into(),
                name: Some("Read file".into()),
                status: "completed".into(),
                input: Some(json!({"path": "main.rs"})),
                output: Some(Value::String("first \nsecond".into())),
                is_final: true,
            }]
        );

        let record = serde_json::to_value(&events[0]).expect("serialize tool record");
        assert_eq!(record["final"], json!(true));
        assert_eq!(record["type"], json!("tool"));

        assert!(
            normalizer
                .push_update(&json!({"sessionUpdate": "tool_call", "toolCallId": ""}))
                .is_empty()
        );
        assert!(
            normalizer
                .push_update(&json!({"sessionUpdate": "tool_call"}))
                .is_empty()
        );
    }

    #[test]
    fn raw_output_wins_over_content_and_failed_status_is_final() {
        let events = normalize_update_event(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": 42,
            "status": "failed",
            "rawOutput": {"error": "boom"},
            "content": [{"content": {"text": "ignored"}}],
        }));
        assert_eq!(events.len(), 1);
        let AcpUpdateEvent::Tool {
            id,
            status,
            output,
            is_final,
            ..
        } = &events[0]
        else {
            panic!("expected final tool record");
        };
        assert_eq!(id, "42");
        assert_eq!(status, "failed");
        assert_eq!(*output, Some(json!({"error": "boom"})));
        assert!(*is_final);
    }

    #[test]
    fn permission_policy_rejects_unoffered_approvals_and_maps_unknown_replies() {
        let options = parse_permission_options(&json!({
            "options": [
                {"optionId": "a1", "kind": "allow_once", "name": "Allow"},
                {"optionId": "", "kind": "allow_once"},
                {"kind": "reject_once"},
                {"optionId": "a2", "kind": "allow_always"},
            ]
        }));
        assert_eq!(options.len(), 2);
        assert_eq!(
            permission_choices(&options),
            vec![
                AcpPermissionDecision::Allow,
                AcpPermissionDecision::Always,
                AcpPermissionDecision::Deny,
            ]
        );

        let allow = evaluate_permission_policy("allow", &options);
        assert_eq!(allow.decision, AcpPermissionDecision::Allow);
        assert!(allow.accepted);
        assert_eq!(
            allow.reply_result(),
            json!({"outcome": "selected", "optionId": "a1"})
        );

        let always = evaluate_permission_policy(AcpPermissionDecision::Always, &options);
        assert_eq!(
            always.outcome,
            AcpPermissionOutcome::Selected("a2".to_owned())
        );

        let deny = evaluate_permission_policy("deny", &options);
        assert_eq!(deny.decision, AcpPermissionDecision::Deny);
        assert_eq!(deny.reply_result(), json!({"outcome": "cancelled"}));

        let unknown = evaluate_permission_policy("something else", &options);
        assert_eq!(unknown.decision, AcpPermissionDecision::Cancel);
        assert_eq!(unknown.outcome, AcpPermissionOutcome::Cancelled);
    }

    #[test]
    fn permission_policy_without_approvals_downgrades_and_replies() {
        let options = parse_permission_options(&json!({
            "options": [{"optionId": "r1", "kind": "reject_once"}]
        }));
        let evaluation = evaluate_permission_policy(AcpPermissionDecision::Allow, &options);
        assert_eq!(evaluation.decision, AcpPermissionDecision::Cancel);
        assert!(!evaluation.accepted);
        assert_eq!(evaluation.reply_result(), json!({"outcome": "cancelled"}));

        let reply = build_permission_reply("perm-1", "deny", &options);
        assert_eq!(reply.id, RequestId::from("perm-1"));
        assert_eq!(
            reply.result,
            Some(json!({"outcome": "selected", "optionId": "r1"}))
        );
        let line = reply.to_line().expect("serialize permission reply");
        assert!(matches!(
            parse_acp_line(&line).expect("parse permission reply"),
            AcpJsonRpcMessage::Response(_)
        ));
    }

    #[test]
    fn session_result_helpers_and_error_builders_are_faithful() {
        let result = json!({"sessionId": "s1", "stopReason": "tool_use"});
        assert_eq!(session_id_from_result(Some(&result)), Some("s1".to_owned()));
        assert_eq!(session_id_from_result(Some(&json!({}))), None);
        assert_eq!(session_id_from_result(None), None);
        assert_eq!(stop_reason_from_result(Some(&result)), "tool_use");
        assert_eq!(
            stop_reason_from_result(Some(&json!({"stopReason": ""}))),
            "end_turn"
        );
        assert_eq!(stop_reason_from_result(None), "end_turn");

        assert_eq!(
            AcpJsonRpcError::unsupported_method("x/y"),
            AcpJsonRpcError::new(METHOD_NOT_FOUND, "Unsupported method: x/y", None)
        );
        assert_eq!(permission_tool_label(None), "tool");
        assert_eq!(
            permission_tool_label(Some(&json!({"title": "Edit"}))),
            "Edit"
        );
        assert_eq!(
            permission_tool_label(Some(&json!({"kind": "write", "title": ""}))),
            "write"
        );
    }

    #[test]
    fn response_roundtrip_and_success_helpers() {
        let response = AcpJsonRpcResponse::success(9, json!({"sessionId": "s9"}));
        let line = response.to_line().expect("serialize");
        assert_eq!(
            line,
            "{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"sessionId\":\"s9\"}}\n"
        );
        let AcpJsonRpcMessage::Response(parsed) = parse_acp_line(&line).expect("parse") else {
            panic!("expected response");
        };
        assert_eq!(parsed, response);

        let missing_result = AcpJsonRpcResponse {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: RequestId::from(10),
            result: None,
            error: None,
        };
        assert_eq!(
            missing_result.into_result().expect("null result"),
            Value::Null
        );

        let failure = AcpJsonRpcResponse::failure(
            11,
            AcpJsonRpcError::invalid_params("path outside session cwd"),
        );
        assert!(failure.to_line().expect("serialize").contains("-32602"));
    }

    #[test]
    fn bare_string_errors_and_non_object_updates_are_handled() {
        let message = parse_acp_line(r#"{"jsonrpc":"2.0","id":3,"error":"harness exploded"}"#)
            .expect("bare string error reply");
        let AcpJsonRpcMessage::Response(response) = message else {
            panic!("expected response");
        };
        assert_eq!(
            response.error,
            Some(AcpJsonRpcError::internal("harness exploded"))
        );
        let err = response.into_result().expect_err("error reply");
        assert_eq!(
            err.to_string(),
            "protocol error: ACP request failed: harness exploded"
        );

        assert!(
            normalize_session_notification("session/update", &json!({"update": null})).is_empty()
        );
        assert!(normalize_session_notification("session/update", &json!({"update": 5})).is_empty());
        assert!(normalize_session_notification(
            "other/method",
            &json!({"update": {"sessionUpdate": "agent_message_chunk", "content": {"text": "x"}}}),
        )
        .is_empty());
    }
}
