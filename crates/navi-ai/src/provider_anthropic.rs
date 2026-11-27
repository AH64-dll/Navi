//! Anthropic Messages API request construction and streaming response decoding.
//!
//! The builder maps the shared [`navi_types::Message`] representation onto the
//! wire format of Anthropic's `/v1/messages` endpoint. The decoder consumes SSE
//! event payloads one at a time, keeps per-block state between calls, and emits
//! [`navi_types::StreamEvent`] values that downstream crates can consume without
//! knowing the Anthropic event grammar.

use navi_types::{
    ContentBlock, Message, NaviError, NaviResult, Role, StopReason, StreamEvent, ToolCall,
    ToolDefinition, ToolResult, Usage,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// How thinking content should be surfaced by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingDisplay {
    /// Return the model's thinking text.
    Summarized,
    /// Omit the text but keep the signature for multi-turn continuity.
    Omitted,
}

impl AnthropicThinkingDisplay {
    /// Returns the wire value for this display mode.
    fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

/// Thinking configuration accepted by the Anthropic Messages API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinking {
    /// Explicitly disables thinking for the request.
    Disabled,
    /// The model decides whether and how much to think.
    Adaptive {
        /// How thinking text appears in the response.
        display: AnthropicThinkingDisplay,
    },
    /// Budget-based thinking for models without adaptive thinking.
    Enabled {
        /// Maximum number of tokens reserved for thinking.
        budget_tokens: u64,
        /// How thinking text appears in the response.
        display: AnthropicThinkingDisplay,
    },
}

/// Tool selection behavior for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicToolChoice {
    /// Let the model choose whether to call a tool.
    Auto,
    /// Require at least one tool call.
    Any,
    /// Prevent tool calls.
    None,
    /// Force one specific tool by name.
    Tool(String),
}

/// Request-level settings for [`build_anthropic_request`].
///
/// Public fields keep the configuration easy to assemble incrementally;
/// [`AnthropicRequestConfig::new`] provides the common case of a model and a
/// token limit with streaming enabled.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicRequestConfig {
    /// Model identifier sent as `model`.
    pub model: String,
    /// Hard upper bound on generated tokens.
    pub max_tokens: u64,
    /// Whether the response is streamed. Defaults to `true`.
    pub stream: bool,
    /// Sampling temperature. Omitted when `None`.
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter. Omitted when `None`.
    pub top_p: Option<f64>,
    /// Top-k sampling parameter. Omitted when `None`.
    pub top_k: Option<u64>,
    /// Custom stop sequences. Omitted when empty.
    pub stop_sequences: Vec<String>,
    /// Extended-thinking settings. Omitted when `None`.
    pub thinking: Option<AnthropicThinking>,
    /// Tool choice settings. Omitted when `None`.
    pub tool_choice: Option<AnthropicToolChoice>,
    /// Optional stable user identifier sent as `metadata.user_id`.
    pub metadata_user_id: Option<String>,
    /// Beta feature identifiers sent as `betas`. Omitted when empty.
    pub betas: Vec<String>,
}

impl AnthropicRequestConfig {
    /// Creates a streaming request configuration with sensible defaults.
    pub fn new(model: impl Into<String>, max_tokens: u64) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            ..Self::default()
        }
    }
}

impl Default for AnthropicRequestConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens: 4096,
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            tool_choice: None,
            metadata_user_id: None,
            betas: Vec::new(),
        }
    }
}

/// Builds an Anthropic Messages API request body as JSON.
///
/// System messages inside `messages` are folded into the top-level `system`
/// prompt, since the Messages API carries system text outside the conversation.
/// Messages that would have no content blocks on the wire are dropped, and tool
/// results are grouped into user messages as Anthropic requires.
pub fn build_anthropic_request(
    cfg: &AnthropicRequestConfig,
    system: Option<&str>,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(cfg.model.clone()));
    body.insert("max_tokens".to_owned(), json!(cfg.max_tokens));
    body.insert("stream".to_owned(), json!(cfg.stream));

    let system_text = join_system_text(system, messages);
    if !system_text.is_empty() {
        body.insert("system".to_owned(), Value::String(system_text));
    }

    body.insert(
        "messages".to_owned(),
        Value::Array(convert_messages(messages)),
    );

    if !tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(tools.iter().map(convert_tool).collect()),
        );
    }

    if let Some(temperature) = cfg.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = cfg.top_p {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(top_k) = cfg.top_k {
        body.insert("top_k".to_owned(), json!(top_k));
    }
    if !cfg.stop_sequences.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(cfg.stop_sequences));
    }
    if !cfg.betas.is_empty() {
        body.insert("betas".to_owned(), json!(cfg.betas));
    }
    if let Some(user_id) = &cfg.metadata_user_id {
        body.insert("metadata".to_owned(), json!({ "user_id": user_id }));
    }
    if let Some(thinking) = &cfg.thinking {
        body.insert("thinking".to_owned(), convert_thinking(*thinking));
    }
    if let Some(tool_choice) = &cfg.tool_choice {
        body.insert("tool_choice".to_owned(), convert_tool_choice(tool_choice));
    }

    Value::Object(body)
}

/// Combines the explicit system prompt with any system messages in the transcript.
fn join_system_text(system: Option<&str>, messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(text) = system
        && !text.trim().is_empty()
    {
        parts.push(text.to_owned());
    }
    for message in messages {
        if message.role == Role::System {
            let text = message.text_content();
            if !text.trim().is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n\n")
}

/// Converts the conversation into Anthropic message objects.
fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut index = 0usize;

    while index < messages.len() {
        let message = &messages[index];
        match message.role {
            Role::System => {
                // System text is carried by the top-level `system` field.
                index += 1;
            }
            Role::User => {
                let blocks = user_content_blocks(&message.content);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
                index += 1;
            }
            Role::Assistant => {
                let blocks = assistant_content_blocks(&message.content);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
                index += 1;
            }
            Role::ToolResult => {
                let mut blocks: Vec<Value> = Vec::new();
                while index < messages.len() && messages[index].role == Role::ToolResult {
                    for block in &messages[index].content {
                        if let ContentBlock::ToolResult(result) = block {
                            blocks.push(convert_tool_result(result));
                        }
                    }
                    index += 1;
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
            }
        }
    }

    out
}

/// Converts user-side content blocks; unsupported blocks are ignored.
fn user_content_blocks(content: &[ContentBlock]) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentBlock::Image { data, mime_type } => blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                },
            })),
            ContentBlock::ToolResult(result) => blocks.push(convert_tool_result(result)),
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) => {}
        }
    }
    blocks
}

/// Converts assistant-side content blocks; unsupported blocks are ignored.
fn assistant_content_blocks(content: &[ContentBlock]) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                if !text.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                let has_signature = signature
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
                if thinking.trim().is_empty() && !has_signature {
                    continue;
                }
                if has_signature {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }));
                } else {
                    // Without a signature the block cannot be replayed as thinking.
                    blocks.push(json!({ "type": "text", "text": thinking }));
                }
            }
            ContentBlock::ToolCall(call) => blocks.push(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.arguments,
            })),
            ContentBlock::Image { .. } | ContentBlock::ToolResult(_) => {}
        }
    }
    blocks
}

/// Converts a shared tool result into an Anthropic `tool_result` block.
fn convert_tool_result(result: &ToolResult) -> Value {
    let content = user_content_blocks(&result.content);
    let mut block = Map::new();
    block.insert("type".to_owned(), json!("tool_result"));
    block.insert("tool_use_id".to_owned(), json!(result.tool_call_id));
    block.insert("content".to_owned(), Value::Array(content));
    if result.is_error {
        block.insert("is_error".to_owned(), json!(true));
    }
    Value::Object(block)
}

/// Converts one tool definition into an Anthropic `BetaTool` object.
///
/// Only schema keywords Anthropic accepts at the root are forwarded; the rest
/// are dropped because the API rejects unknown root-level JSON Schema keys.
fn convert_tool(tool: &ToolDefinition) -> Value {
    let input_schema = match &tool.parameters {
        Value::Object(schema) => {
            let mut accepted = Map::new();
            accepted.insert("type".to_owned(), json!("object"));
            accepted.insert(
                "properties".to_owned(),
                schema
                    .get("properties")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
            accepted.insert(
                "required".to_owned(),
                schema.get("required").cloned().unwrap_or_else(|| json!([])),
            );
            for key in ["additionalProperties", "$schema", "$defs", "definitions"] {
                if let Some(value) = schema.get(key) {
                    accepted.insert(key.to_owned(), value.clone());
                }
            }
            Value::Object(accepted)
        }
        other => other.clone(),
    };

    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": input_schema,
    })
}

/// Converts the thinking configuration into its wire representation.
fn convert_thinking(thinking: AnthropicThinking) -> Value {
    match thinking {
        AnthropicThinking::Disabled => json!({ "type": "disabled" }),
        AnthropicThinking::Adaptive { display } => {
            json!({ "type": "adaptive", "display": display.as_str() })
        }
        AnthropicThinking::Enabled {
            budget_tokens,
            display,
        } => json!({
            "type": "enabled",
            "budget_tokens": budget_tokens,
            "display": display.as_str(),
        }),
    }
}

/// Converts a tool choice into its wire representation.
fn convert_tool_choice(tool_choice: &AnthropicToolChoice) -> Value {
    match tool_choice {
        AnthropicToolChoice::Auto => json!({ "type": "auto" }),
        AnthropicToolChoice::Any => json!({ "type": "any" }),
        AnthropicToolChoice::None => json!({ "type": "none" }),
        AnthropicToolChoice::Tool(name) => json!({ "type": "tool", "name": name }),
    }
}

/// Maps an Anthropic stop reason onto the shared, provider-independent reason.
fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        // Refusals, safety stops, and unknown new reasons are surfaced as errors.
        _ => StopReason::Error,
    }
}

/// Parses one SSE `data:` payload.
fn parse_data(data: &str) -> NaviResult<Value> {
    serde_json::from_str(data)
        .map_err(|error| NaviError::Protocol(format!("invalid Anthropic event data: {error}")))
}

/// Reads a message from a provider-level error payload.
fn provider_error_message(value: &Value) -> String {
    if let Value::String(text) = value
        && !text.is_empty()
    {
        return text.clone();
    }
    let container = value.get("error").unwrap_or(value);
    container
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| container.get("error").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .unwrap_or("Anthropic stream error")
        .to_owned()
}

/// Merges streamed `input_json_delta` fragments into the tool's initial input.
fn merge_tool_arguments(input: &Value, partial_json: &str) -> Value {
    let mut arguments = match input {
        Value::Object(_) => input.clone(),
        _ => Value::Object(Map::new()),
    };
    if partial_json.trim().is_empty() {
        return arguments;
    }
    if let Value::Object(streamed) = crate::streaming_json::parse_tool_arguments(partial_json)
        && let Value::Object(base) = &mut arguments
    {
        for (key, value) in streamed {
            base.insert(key, value);
        }
    }
    arguments
}

/// State kept for one open Anthropic content block.
#[derive(Debug)]
struct OpenBlock {
    /// Position of the block in the accumulated assistant message.
    content_index: usize,
    /// Which kind of block is currently receiving deltas.
    kind: OpenBlockKind,
}

/// Per-kind state for an open content block.
#[derive(Debug)]
enum OpenBlockKind {
    Text,
    Thinking,
    ToolUse {
        id: String,
        name: String,
        input: Value,
        partial_json: String,
    },
}

/// Incremental decoder for Anthropic Messages API SSE events.
///
/// One decoder represents one response stream. Feed it the `event` and `data`
/// fields of each dispatched SSE event; the returned events are ready to be
/// forwarded to consumers. After `message_stop` the decoder is finished and
/// ignores further events.
#[derive(Debug, Default)]
pub struct AnthropicStreamDecoder {
    finished: bool,
    open_blocks: BTreeMap<u64, OpenBlock>,
    content: Vec<ContentBlock>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    response_id: Option<String>,
    response_model: Option<String>,
}

impl AnthropicStreamDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the identifier reported by `message_start`, if any.
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    /// Returns the model that actually answered, if reported.
    pub fn response_model(&self) -> Option<&str> {
        self.response_model.as_deref()
    }

    /// Returns the token usage accumulated so far.
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Returns the stop reason reported by `message_delta`, if any.
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    /// Consumes one SSE event and returns the shared events it produced.
    ///
    /// Malformed JSON is reported as [`NaviError::Protocol`]; a provider-level
    /// `error` event is reported as [`NaviError::Provider`]. Events outside the
    /// Messages API grammar are ignored.
    pub fn push_event(&mut self, event: &str, data: &str) -> NaviResult<Vec<StreamEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        match event {
            "message_start" => self.on_message_start(data),
            "content_block_start" => self.on_content_block_start(data),
            "content_block_delta" => self.on_content_block_delta(data),
            "content_block_stop" => self.on_content_block_stop(data),
            "message_delta" => self.on_message_delta(data),
            "message_stop" => Ok(vec![self.on_message_stop()]),
            "error" => Err(NaviError::Provider(provider_error_message(&parse_data(
                data,
            )?))),
            _ => Ok(Vec::new()),
        }
    }

    /// Records identifiers and initial token usage from `message_start`.
    fn on_message_start(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        let payload = parse_data(data)?;
        if let Some(message) = payload.get("message") {
            if let Some(id) = message.get("id").and_then(Value::as_str) {
                self.response_id = Some(id.to_owned());
            }
            if let Some(model) = message.get("model").and_then(Value::as_str) {
                self.response_model = Some(model.to_owned());
            }
            if let Some(usage) = message.get("usage") {
                self.apply_usage(usage);
            }
        }
        Ok(vec![StreamEvent::Start])
    }

    /// Opens a content block and emits the tool-call start event when needed.
    fn on_content_block_start(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        let payload = parse_data(data)?;
        let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0);
        if self.open_blocks.contains_key(&index) {
            return Ok(Vec::new());
        }
        let Some(block) = payload.get("content_block") else {
            return Ok(Vec::new());
        };

        let (content, kind) = match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                (ContentBlock::Text { text }, OpenBlockKind::Text)
            }
            "thinking" => {
                let thinking = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let signature = if signature.is_empty() {
                    None
                } else {
                    Some(signature)
                };
                (
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    },
                    OpenBlockKind::Thinking,
                )
            }
            "redacted_thinking" => {
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                (
                    ContentBlock::Thinking {
                        thinking: "[Reasoning redacted]".to_owned(),
                        signature: Some(data),
                    },
                    OpenBlockKind::Thinking,
                )
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let input = match block.get("input") {
                    Some(input @ Value::Object(_)) => input.clone(),
                    _ => Value::Object(Map::new()),
                };
                let arguments = input.clone();
                (
                    ContentBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments,
                    }),
                    OpenBlockKind::ToolUse {
                        id,
                        name,
                        input,
                        partial_json: String::new(),
                    },
                )
            }
            _ => return Ok(Vec::new()),
        };

        self.content.push(content);
        let content_index = self.content.len() - 1;
        let start_event = match &kind {
            OpenBlockKind::ToolUse { id, name, .. } => Some(StreamEvent::ToolCallStart {
                content_index,
                id: id.clone(),
                name: name.clone(),
            }),
            _ => None,
        };
        self.open_blocks.insert(
            index,
            OpenBlock {
                content_index,
                kind,
            },
        );
        Ok(start_event.into_iter().collect())
    }

    /// Applies a delta to the open block and emits the matching shared event.
    fn on_content_block_delta(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        let payload = parse_data(data)?;
        let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(delta) = payload.get("delta") else {
            return Ok(Vec::new());
        };
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(open) = self.open_blocks.get_mut(&index) else {
            return Ok(Vec::new());
        };
        let content_index = open.content_index;

        match delta_type {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(ContentBlock::Text { text: target }) =
                    self.content.get_mut(content_index)
                {
                    target.push_str(text);
                }
                Ok(vec![StreamEvent::TextDelta {
                    content_index,
                    delta: text.to_owned(),
                }])
            }
            "thinking_delta" => {
                let thinking = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(ContentBlock::Thinking {
                    thinking: target, ..
                }) = self.content.get_mut(content_index)
                {
                    target.push_str(thinking);
                }
                Ok(vec![StreamEvent::ThinkingDelta {
                    content_index,
                    delta: thinking.to_owned(),
                }])
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let OpenBlockKind::ToolUse { partial_json, .. } = &mut open.kind else {
                    return Ok(Vec::new());
                };
                partial_json.push_str(partial);
                Ok(vec![StreamEvent::ToolCallDelta {
                    content_index,
                    delta: partial.to_owned(),
                }])
            }
            "signature_delta" => {
                let signature = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(ContentBlock::Thinking {
                    signature: target, ..
                }) = self.content.get_mut(content_index)
                {
                    let mut combined = target.take().unwrap_or_default();
                    combined.push_str(signature);
                    *target = Some(combined);
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Finalizes a block, resolving streamed tool arguments on close.
    fn on_content_block_stop(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        let payload = parse_data(data)?;
        let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0);
        let Some(open) = self.open_blocks.remove(&index) else {
            return Ok(Vec::new());
        };
        let content_index = open.content_index;
        let OpenBlockKind::ToolUse {
            id,
            name,
            input,
            partial_json,
        } = open.kind
        else {
            return Ok(Vec::new());
        };

        let arguments = merge_tool_arguments(&input, &partial_json);
        let tool_call = match self.content.get_mut(content_index) {
            Some(ContentBlock::ToolCall(call)) => {
                call.arguments = arguments;
                call.clone()
            }
            _ => ToolCall {
                id,
                name,
                arguments,
            },
        };
        Ok(vec![StreamEvent::ToolCallEnd {
            content_index,
            tool_call,
        }])
    }

    /// Records stop reason and usage from `message_delta`.
    fn on_message_delta(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        let payload = parse_data(data)?;
        if let Some(delta) = payload.get("delta")
            && let Some(reason) = delta.get("stop_reason").and_then(Value::as_str)
        {
            self.stop_reason = Some(map_stop_reason(reason));
        }
        if let Some(usage) = payload.get("usage") {
            self.apply_usage(usage);
        }
        Ok(Vec::new())
    }

    /// Builds the final `Done` event from the accumulated state.
    fn on_message_stop(&mut self) -> StreamEvent {
        self.finished = true;
        self.open_blocks.clear();
        let reason = self.stop_reason.unwrap_or(StopReason::Stop);
        let message = Message {
            role: Role::Assistant,
            content: std::mem::take(&mut self.content),
            timestamp_ms: 0,
            stop_reason: Some(reason),
            usage: Some(self.usage.clone()),
            metadata: BTreeMap::new(),
        };
        StreamEvent::Done { reason, message }
    }

    /// Updates usage fields that are present, preserving earlier values otherwise.
    fn apply_usage(&mut self, usage: &Value) {
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.usage.input_tokens = value;
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = value;
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.usage.cache_read_tokens = value;
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_write_tokens = value;
        }
        self.usage.total_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.output_tokens)
            .saturating_add(self.usage.cache_read_tokens)
            .saturating_add(self.usage.cache_write_tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as TestMap;

    fn message(role: Role, content: Vec<ContentBlock>) -> Message {
        Message {
            role,
            content,
            timestamp_ms: 1,
            stop_reason: None,
            usage: None,
            metadata: TestMap::new(),
        }
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text {
            text: value.to_owned(),
        }
    }

    fn push(decoder: &mut AnthropicStreamDecoder, event: &str, data: &str) -> Vec<StreamEvent> {
        decoder
            .push_event(event, data)
            .expect("test event data is valid JSON")
    }

    fn assert_done(events: &[StreamEvent]) -> (&StopReason, &Message) {
        match events {
            [StreamEvent::Done { reason, message }] => (reason, message),
            other => panic!("expected one done event, got {other:?}"),
        }
    }

    #[test]
    fn build_request_basic_text_and_system() {
        let cfg = AnthropicRequestConfig::new("claude-sonnet-4-5", 1024);
        let messages = vec![Message::user_text("hello", 1)];

        let body = build_anthropic_request(&cfg, Some("be concise"), &messages, &[]);

        assert_eq!(body["model"], json!("claude-sonnet-4-5"));
        assert_eq!(body["max_tokens"], json!(1024));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["system"], json!("be concise"));
        assert_eq!(
            body["messages"],
            json!([{ "role": "user", "content": [{ "type": "text", "text": "hello" }] }])
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_request_converts_all_content_kinds_and_tools() {
        let cfg = AnthropicRequestConfig::new("claude", 2048);
        let tools = vec![ToolDefinition {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "anyOf": [{ "type": "object" }],
            }),
        }];
        let messages = vec![
            message(
                Role::User,
                vec![
                    text("look at this"),
                    ContentBlock::Image {
                        data: "YWJj".to_owned(),
                        mime_type: "image/png".to_owned(),
                    },
                ],
            ),
            message(
                Role::Assistant,
                vec![
                    text("checking"),
                    ContentBlock::Thinking {
                        thinking: "step one".to_owned(),
                        signature: Some("sig".to_owned()),
                    },
                    ContentBlock::ToolCall(ToolCall {
                        id: "toolu_1".to_owned(),
                        name: "read".to_owned(),
                        arguments: json!({ "path": "Cargo.toml" }),
                    }),
                ],
            ),
            message(
                Role::ToolResult,
                vec![ContentBlock::ToolResult(ToolResult {
                    tool_call_id: "toolu_1".to_owned(),
                    tool_name: "read".to_owned(),
                    content: vec![text("ok")],
                    is_error: false,
                    details: None,
                })],
            ),
        ];

        let body = build_anthropic_request(&cfg, None, &messages, &tools);

        assert_eq!(body.get("system"), None);
        assert_eq!(
            body["tools"],
            json!([{
                "name": "read",
                "description": "Read a file",
                "input_schema": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                },
            }])
        );
        let wire = body["messages"].as_array().expect("messages is an array");
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[0]["role"], json!("user"));
        assert_eq!(wire[0]["content"][1]["type"], json!("image"));
        assert_eq!(
            wire[0]["content"][1]["source"]["media_type"],
            json!("image/png")
        );
        assert_eq!(wire[1]["role"], json!("assistant"));
        assert_eq!(wire[1]["content"][1]["type"], json!("thinking"));
        assert_eq!(wire[1]["content"][1]["signature"], json!("sig"));
        assert_eq!(wire[1]["content"][2]["type"], json!("tool_use"));
        assert_eq!(wire[1]["content"][2]["id"], json!("toolu_1"));
        assert_eq!(wire[2]["role"], json!("user"));
        assert_eq!(wire[2]["content"][0]["type"], json!("tool_result"));
        assert_eq!(wire[2]["content"][0]["tool_use_id"], json!("toolu_1"));
    }

    #[test]
    fn build_request_folds_system_messages_and_skips_empty_content() {
        let cfg = AnthropicRequestConfig::default();
        let messages = vec![
            message(Role::System, vec![text("from transcript")]),
            message(Role::User, vec![text("   ")]),
            message(Role::Assistant, vec![]),
            message(
                Role::Assistant,
                vec![ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                }],
            ),
        ];

        let body = build_anthropic_request(&cfg, Some("base"), &messages, &[]);

        assert_eq!(body["system"], json!("base\n\nfrom transcript"));
        assert_eq!(body["messages"], json!([]));
        assert_eq!(body["max_tokens"], json!(4096));
    }

    #[test]
    fn build_request_serializes_optional_config_fields() {
        let mut cfg = AnthropicRequestConfig::new("m", 10);
        cfg.temperature = Some(0.5);
        cfg.top_p = Some(0.9);
        cfg.top_k = Some(40);
        cfg.stop_sequences = vec!["END".to_owned()];
        cfg.thinking = Some(AnthropicThinking::Enabled {
            budget_tokens: 2048,
            display: AnthropicThinkingDisplay::Omitted,
        });
        cfg.tool_choice = Some(AnthropicToolChoice::Tool("read".to_owned()));
        cfg.metadata_user_id = Some("user-1".to_owned());
        cfg.betas = vec!["beta-x".to_owned()];
        cfg.stream = false;

        let body = build_anthropic_request(&cfg, None, &[], &[]);

        assert_eq!(body["stream"], json!(false));
        assert_eq!(body["temperature"], json!(0.5));
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["top_k"], json!(40));
        assert_eq!(body["stop_sequences"], json!(["END"]));
        assert_eq!(body["betas"], json!(["beta-x"]));
        assert_eq!(body["metadata"], json!({ "user_id": "user-1" }));
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "budget_tokens": 2048, "display": "omitted" })
        );
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "tool", "name": "read" })
        );
        assert_eq!(
            build_anthropic_request(
                &AnthropicRequestConfig {
                    thinking: Some(AnthropicThinking::Disabled),
                    tool_choice: Some(AnthropicToolChoice::Any),
                    ..AnthropicRequestConfig::new("m", 1)
                },
                None,
                &[],
                &[]
            )["thinking"],
            json!({ "type": "disabled" })
        );
    }

    #[test]
    fn decoder_streams_text_and_builds_final_message() {
        let mut decoder = AnthropicStreamDecoder::new();

        assert_eq!(
            push(
                &mut decoder,
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":7,"output_tokens":0}}}"#
            ),
            vec![StreamEvent::Start]
        );
        assert!(push(
            &mut decoder,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hi"}}"#
        )
        .is_empty());
        assert_eq!(
            push(
                &mut decoder,
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}"#
            ),
            vec![StreamEvent::TextDelta {
                content_index: 0,
                delta: " there".to_owned(),
            }]
        );
        assert!(
            push(
                &mut decoder,
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#
            )
            .is_empty()
        );
        assert!(push(
            &mut decoder,
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#
        )
        .is_empty());

        let done = push(&mut decoder, "message_stop", r#"{"type":"message_stop"}"#);
        let (reason, message) = assert_done(&done);
        assert_eq!(*reason, StopReason::Stop);
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.text_content(), "Hi there");
        assert_eq!(message.stop_reason, Some(StopReason::Stop));
        let usage = message.usage.as_ref().expect("usage is present");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 10);
        assert_eq!(decoder.response_id(), Some("msg_1"));
        assert_eq!(decoder.response_model(), Some("claude"));
    }

    #[test]
    fn decoder_streams_tool_call_arguments() {
        let mut decoder = AnthropicStreamDecoder::new();
        assert_eq!(
            push(
                &mut decoder,
                "content_block_start",
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_9","name":"write","input":{}}}"#
            ),
            vec![StreamEvent::ToolCallStart {
                content_index: 0,
                id: "toolu_9".to_owned(),
                name: "write".to_owned(),
            }]
        );

        assert_eq!(
            push(
                &mut decoder,
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#
            ),
            vec![StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\":".to_owned(),
            }]
        );
        assert_eq!(
            push(
                &mut decoder,
                "content_block_delta",
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#
            ),
            vec![StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "\"a.txt\"}".to_owned(),
            }]
        );

        let ended = push(
            &mut decoder,
            "content_block_stop",
            r#"{"type":"content_block_stop","index":2}"#,
        );
        match ended.as_slice() {
            [
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                },
            ] => {
                assert_eq!(*content_index, 0);
                assert_eq!(tool_call.id, "toolu_9");
                assert_eq!(tool_call.name, "write");
                assert_eq!(tool_call.arguments, json!({ "path": "a.txt" }));
            }
            other => panic!("expected one tool call end event, got {other:?}"),
        }

        push(
            &mut decoder,
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#,
        );
        let done = push(&mut decoder, "message_stop", r#"{"type":"message_stop"}"#);
        let (reason, message) = assert_done(&done);
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.content.len(), 1);
        assert!(
            matches!(&message.content[0], ContentBlock::ToolCall(call) if call.arguments == json!({ "path": "a.txt" }))
        );
    }

    #[test]
    fn decoder_accumulates_thinking_and_signature() {
        let mut decoder = AnthropicStreamDecoder::new();
        assert!(push(
            &mut decoder,
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"Let me","signature":"sig"}}"#
        )
        .is_empty());
        assert_eq!(
            push(
                &mut decoder,
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" think"}}"#
            ),
            vec![StreamEvent::ThinkingDelta {
                content_index: 0,
                delta: " think".to_owned(),
            }]
        );
        assert!(push(
            &mut decoder,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"_tail"}}"#
        )
        .is_empty());
        push(
            &mut decoder,
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        );

        let done = push(&mut decoder, "message_stop", r#"{"type":"message_stop"}"#);
        let (_, message) = assert_done(&done);
        assert_eq!(
            message.content,
            vec![ContentBlock::Thinking {
                thinking: "Let me think".to_owned(),
                signature: Some("sig_tail".to_owned()),
            }]
        );
    }

    #[test]
    fn decoder_ignores_unknown_events_and_reports_errors() {
        let mut decoder = AnthropicStreamDecoder::new();
        assert!(push(&mut decoder, "ping", r#"{"type":"ping"}"#).is_empty());
        assert!(
            decoder
                .push_event("content_block_delta", "not json")
                .is_err()
        );

        let error = decoder
            .push_event(
                "error",
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            )
            .expect_err("provider errors are returned");
        assert_eq!(error, NaviError::Provider("Overloaded".to_owned()));
    }

    #[test]
    fn decoder_ignores_deltas_for_unknown_block_indices() {
        let mut decoder = AnthropicStreamDecoder::new();
        assert!(push(
            &mut decoder,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":9,"delta":{"type":"text_delta","text":"silently dropped"}}"#
        )
        .is_empty());
        assert!(
            push(
                &mut decoder,
                "content_block_stop",
                r#"{"type":"content_block_stop","index":9}"#
            )
            .is_empty()
        );

        let done = push(&mut decoder, "message_stop", r#"{"type":"message_stop"}"#);
        let (_, message) = assert_done(&done);
        assert!(message.content.is_empty());
    }

    #[test]
    fn decoder_maps_every_stop_reason() {
        let cases = [
            ("end_turn", StopReason::Stop),
            ("stop_sequence", StopReason::Stop),
            ("pause_turn", StopReason::Stop),
            ("max_tokens", StopReason::Length),
            ("tool_use", StopReason::ToolUse),
            ("refusal", StopReason::Error),
            ("future_reason", StopReason::Error),
        ];
        for (raw, expected) in cases {
            let mut decoder = AnthropicStreamDecoder::new();
            let payload = format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"{raw}"}},"usage":{{}}}}"#
            );
            push(&mut decoder, "message_delta", &payload);
            let done = push(&mut decoder, "message_stop", r#"{"type":"message_stop"}"#);
            let (reason, message) = assert_done(&done);
            assert_eq!(*reason, expected, "raw reason: {raw}");
            assert_eq!(message.stop_reason, Some(expected));
        }
    }

    #[test]
    fn decoder_preserves_usage_from_message_start_and_ignores_late_events() {
        let mut decoder = AnthropicStreamDecoder::default();
        push(
            &mut decoder,
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}"#,
        );
        push(
            &mut decoder,
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
        );

        let done = push(&mut decoder, "message_stop", r#"{"type":"message_stop"}"#);
        let (_, message) = assert_done(&done);
        let usage = message.usage.as_ref().expect("usage is present");
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cache_read_tokens, 2);
        assert_eq!(usage.cache_write_tokens, 1);
        assert_eq!(usage.total_tokens, 12);
        assert_eq!(decoder.usage().total_tokens, 12);
        assert_eq!(decoder.stop_reason(), Some(StopReason::Stop));

        assert!(push(&mut decoder, "message_start", r#"{"message":{}}"#).is_empty());
        assert!(push(&mut decoder, "message_stop", r#"{}"#).is_empty());
    }
}
