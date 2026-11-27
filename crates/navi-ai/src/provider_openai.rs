//! OpenAI-compatible Chat Completions request builder and SSE stream decoder.
//!
//! The builder turns the shared [`navi_types::Message`] model into the JSON body
//! of a streaming `/chat/completions` request. The decoder consumes the `data:`
//! payloads of that stream, keeps incremental tool-call arguments in order, and
//! emits [`navi_types::StreamEvent`] values until the `[DONE]` sentinel closes
//! the turn.

use crate::sse::is_done_event;
use crate::streaming_json::parse_tool_arguments;
use navi_types::{
    ContentBlock, Message, NaviError, NaviResult, Role, StopReason, StreamEvent, ToolCall,
    ToolDefinition, Usage,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, btree_map::Entry};

/// Sampling and replay options for an OpenAI Chat Completions request.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiRequestConfig {
    /// Model identifier sent as `model`.
    pub model: String,
    /// Maximum completion tokens, sent as `max_completion_tokens`.
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling probability mass.
    pub top_p: Option<f64>,
    /// Reasoning effort for models that accept `reasoning_effort`.
    pub reasoning_effort: Option<String>,
    /// Tool choice override, either a string such as `"auto"` or a named tool object.
    pub tool_choice: Option<Value>,
    /// Whether to request token accounting in the final stream chunk.
    pub include_usage: bool,
}

impl OpenAiRequestConfig {
    /// Creates a config for `model` with streaming usage enabled.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }
}

impl Default for OpenAiRequestConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            tool_choice: None,
            include_usage: true,
        }
    }
}

/// Builds the JSON request body for a streaming Chat Completions call.
pub fn build_openai_request(
    cfg: &OpenAiRequestConfig,
    system: Option<&str>,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(cfg.model.clone()));
    body.insert("stream".to_owned(), Value::Bool(true));
    if cfg.include_usage {
        body.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );
    }
    if let Some(max_tokens) = cfg.max_tokens {
        body.insert("max_completion_tokens".to_owned(), json!(max_tokens));
    }
    if let Some(temperature) = cfg.temperature.filter(|value| value.is_finite()) {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = cfg.top_p.filter(|value| value.is_finite()) {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(effort) = &cfg.reasoning_effort {
        body.insert("reasoning_effort".to_owned(), Value::String(effort.clone()));
    }

    let mut converted = Vec::with_capacity(messages.len() + usize::from(system.is_some()));
    if let Some(text) = system.filter(|text| !text.is_empty()) {
        converted.push(json!({ "role": "system", "content": text }));
    }
    for message in messages {
        convert_message(message, &mut converted);
    }
    body.insert("messages".to_owned(), Value::Array(converted));

    if !tools.is_empty() {
        let converted_tools: Vec<Value> = tools.iter().map(convert_tool).collect();
        body.insert("tools".to_owned(), Value::Array(converted_tools));
    }
    if let Some(choice) = &cfg.tool_choice {
        body.insert("tool_choice".to_owned(), choice.clone());
    }
    Value::Object(body)
}

/// Converts one tool definition into a Chat Completions function tool.
fn convert_tool(tool: &ToolDefinition) -> Value {
    let mut function = Map::new();
    function.insert("name".to_owned(), Value::String(tool.name.clone()));
    function.insert(
        "description".to_owned(),
        Value::String(tool.description.clone()),
    );
    function.insert("parameters".to_owned(), tool.parameters.clone());

    let mut wrapper = Map::new();
    wrapper.insert("type".to_owned(), Value::String("function".to_owned()));
    wrapper.insert("function".to_owned(), Value::Object(function));
    Value::Object(wrapper)
}

/// Joins the text blocks of a message with newlines.
fn collect_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Joins the thinking blocks of a message with newlines.
fn collect_thinking(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                Some(thinking.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Builds `image_url` parts for every image block in a message.
fn collect_images(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data, mime_type } => Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
            })),
            _ => None,
        })
        .collect()
}

/// Appends the Chat Completions messages produced by one shared message.
fn convert_message(message: &Message, out: &mut Vec<Value>) {
    match message.role {
        Role::System => {
            let text = collect_text(&message.content);
            if !text.is_empty() {
                out.push(json!({ "role": "system", "content": text }));
            }
        }
        Role::User => {
            let images = collect_images(&message.content);
            if images.is_empty() {
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    out.push(json!({ "role": "user", "content": text }));
                }
            } else {
                let mut parts = Vec::with_capacity(images.len() + 1);
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    parts.push(json!({ "type": "text", "text": text }));
                }
                parts.extend(images);
                out.push(json!({ "role": "user", "content": parts }));
            }
        }
        Role::Assistant => {
            let text = collect_text(&message.content);
            let thinking = collect_thinking(&message.content);
            let calls: Vec<Value> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall(call) => Some(json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned()),
                        },
                    })),
                    _ => None,
                })
                .collect();
            if text.is_empty() && thinking.is_empty() && calls.is_empty() {
                return;
            }

            let mut assistant = Map::new();
            assistant.insert("role".to_owned(), Value::String("assistant".to_owned()));
            let content = if text.is_empty() && !calls.is_empty() {
                Value::String(String::new())
            } else if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };
            assistant.insert("content".to_owned(), content);
            if !thinking.is_empty() {
                assistant.insert("reasoning_content".to_owned(), Value::String(thinking));
            }
            if !calls.is_empty() {
                assistant.insert("tool_calls".to_owned(), Value::Array(calls));
            }
            out.push(Value::Object(assistant));
        }
        Role::ToolResult => {
            for block in &message.content {
                let ContentBlock::ToolResult(result) = block else {
                    continue;
                };
                let text = collect_text(&result.content);
                let images = collect_images(&result.content);
                let content = if text.is_empty() && images.is_empty() {
                    "(no tool output)".to_owned()
                } else if text.is_empty() {
                    "(see attached image)".to_owned()
                } else {
                    text
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id,
                    "content": content,
                }));
                if !images.is_empty() {
                    let mut parts = vec![json!({
                        "type": "text",
                        "text": "Attached image(s) from tool result:",
                    })];
                    parts.extend(images);
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
        }
    }
}

/// Field names used by OpenAI-compatible providers for reasoning deltas.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

/// Accumulated state of one streamed tool call.
#[derive(Debug, Clone)]
struct ToolState {
    /// Position of this call in the assistant content.
    content_index: usize,
    /// Call identifier from the first delta that carried one.
    id: String,
    /// Function name accumulated from name deltas.
    name: String,
    /// JSON argument text accumulated across deltas.
    arguments: String,
    /// Whether the start event has been emitted.
    started: bool,
}

/// Incremental decoder for OpenAI Chat Completions SSE data payloads.
///
/// Feed each `data:` line (or a whole SSE frame) to [`Self::push_chunk`]. The
/// stream is complete once `[DONE]` is seen; [`Self::finish`] closes a stream
/// that ends without that sentinel.
#[derive(Debug, Default)]
pub struct OpenAiStreamDecoder {
    started: bool,
    finished: bool,
    next_index: usize,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    blocks: Vec<Option<ContentBlock>>,
    tools: BTreeMap<usize, ToolState>,
    tool_ids: BTreeMap<String, usize>,
    finish_reason: Option<StopReason>,
    error_message: Option<String>,
    usage: Usage,
    response_id: Option<String>,
    response_model: Option<String>,
}

impl OpenAiStreamDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns whether `[DONE]` or [`Self::finish`] already closed the stream.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Consumes one chunk of SSE text and returns the events it completes.
    ///
    /// Both bare JSON payloads and lines prefixed with `data:` are accepted.
    /// Malformed JSON is reported as a [`NaviError::Protocol`] error.
    pub fn push_chunk(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        for payload in sse_payloads(data) {
            if is_done_event(payload) {
                self.emit_start(&mut events);
                self.finalize(&mut events);
                break;
            }
            let chunk: Value = serde_json::from_str(payload).map_err(|error| {
                NaviError::Protocol(format!("invalid OpenAI stream chunk: {error}"))
            })?;
            self.emit_start(&mut events);
            self.consume_chunk(&chunk, &mut events);
        }
        Ok(events)
    }

    /// Closes the stream without requiring a `[DONE]` payload.
    ///
    /// Calling it after completion is a no-op.
    pub fn finish(&mut self) -> NaviResult<Vec<StreamEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        self.emit_start(&mut events);
        self.finalize(&mut events);
        Ok(events)
    }

    /// Emits the single `start` event for a stream.
    fn emit_start(&mut self, events: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            events.push(StreamEvent::Start);
        }
    }

    /// Applies one parsed chunk to the decoder state.
    fn consume_chunk(&mut self, chunk: &Value, events: &mut Vec<StreamEvent>) {
        if let Some(id) = chunk.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(model) = chunk
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
        {
            self.response_model = Some(model.to_owned());
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage = parse_usage(usage);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return;
        };
        // Some providers put accounting on the choice instead of the chunk.
        if is_empty_usage(&self.usage)
            && let Some(usage) = choice.get("usage")
        {
            self.usage = parse_usage(usage);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            let (stop_reason, error) = map_finish_reason(reason);
            self.finish_reason = Some(stop_reason);
            self.error_message = error;
        }

        let Some(delta) = choice.get("delta") else {
            return;
        };
        self.consume_reasoning(delta, events);
        self.consume_content(delta, events);
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                self.consume_tool_call(tool_call, events);
            }
        }
    }

    /// Appends the first non-empty reasoning field of a delta to the thinking block.
    fn consume_reasoning(&mut self, delta: &Value, events: &mut Vec<StreamEvent>) {
        for field in REASONING_FIELDS {
            let Some(text) = delta.get(field).and_then(Value::as_str) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let index = match self.thinking_index {
                Some(index) => index,
                None => {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.thinking_index = Some(index);
                    self.blocks.push(Some(ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: Some(field.to_owned()),
                    }));
                    index
                }
            };
            if let Some(ContentBlock::Thinking { thinking, .. }) =
                self.blocks.get_mut(index).and_then(Option::as_mut)
            {
                thinking.push_str(text);
            }
            events.push(StreamEvent::ThinkingDelta {
                content_index: index,
                delta: text.to_owned(),
            });
            break;
        }
    }

    /// Appends a text delta to the single text block of the response.
    fn consume_content(&mut self, delta: &Value, events: &mut Vec<StreamEvent>) {
        let Some(text) = delta.get("content").and_then(Value::as_str) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let index = match self.text_index {
            Some(index) => index,
            None => {
                let index = self.next_index;
                self.next_index += 1;
                self.text_index = Some(index);
                self.blocks.push(Some(ContentBlock::Text {
                    text: String::new(),
                }));
                index
            }
        };
        if let Some(ContentBlock::Text { text: block }) =
            self.blocks.get_mut(index).and_then(Option::as_mut)
        {
            block.push_str(text);
        }
        events.push(StreamEvent::TextDelta {
            content_index: index,
            delta: text.to_owned(),
        });
    }

    /// Applies one incremental `tool_calls` entry, creating the call on first sight.
    fn consume_tool_call(&mut self, item: &Value, events: &mut Vec<StreamEvent>) {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let index = self.tool_call_index(item, id);
        let content_index = self.next_index;
        let created = match self.tools.entry(index) {
            Entry::Occupied(_) => false,
            Entry::Vacant(slot) => {
                slot.insert(ToolState {
                    content_index,
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    started: false,
                });
                true
            }
        };
        if created {
            self.next_index += 1;
            self.blocks.push(None);
        }
        if !id.is_empty() {
            self.tool_ids.insert(id.to_owned(), index);
        }

        let function = item.get("function");
        let custom = item.get("custom");
        let name = function
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .or_else(|| {
                custom
                    .and_then(|value| value.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        let arguments = function
            .and_then(|value| value.get("arguments"))
            .and_then(Value::as_str)
            .or_else(|| {
                custom
                    .and_then(|value| value.get("input"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");

        let Some(state) = self.tools.get_mut(&index) else {
            return;
        };
        if !id.is_empty() {
            state.id = id.to_owned();
        }
        if state.name.is_empty() && !name.is_empty() {
            state.name = name.to_owned();
        }
        if !arguments.is_empty() {
            state.arguments.push_str(arguments);
        }
        let content_index = state.content_index;
        if !state.started {
            state.started = true;
            events.push(StreamEvent::ToolCallStart {
                content_index,
                id: state.id.clone(),
                name: state.name.clone(),
            });
        }
        if !arguments.is_empty() {
            events.push(StreamEvent::ToolCallDelta {
                content_index,
                delta: arguments.to_owned(),
            });
        }
    }

    /// Resolves the state key for a tool-call delta.
    fn tool_call_index(&self, item: &Value, id: &str) -> usize {
        if let Some(index) = item.get("index").and_then(Value::as_u64) {
            return index as usize;
        }
        if let Some(index) = self.tool_ids.get(id) {
            return *index;
        }
        // Index-less calls get a key that cannot collide with a provider index.
        usize::MAX.saturating_sub(self.tools.len())
    }

    /// Emits pending tool calls, the final message, and the terminal event.
    fn finalize(&mut self, events: &mut Vec<StreamEvent>) {
        if self.finished {
            return;
        }
        self.finished = true;

        for state in std::mem::take(&mut self.tools).into_values() {
            let call = ToolCall {
                id: state.id,
                name: state.name,
                arguments: parse_tool_arguments(&state.arguments),
            };
            if let Some(slot) = self.blocks.get_mut(state.content_index) {
                *slot = Some(ContentBlock::ToolCall(call.clone()));
            }
            events.push(StreamEvent::ToolCallEnd {
                content_index: state.content_index,
                tool_call: call,
            });
        }

        let content: Vec<ContentBlock> = self.blocks.iter().flatten().cloned().collect();
        let has_tools = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        let reason = match self.finish_reason {
            Some(reason) => reason,
            None if has_tools => StopReason::ToolUse,
            None => StopReason::Stop,
        };

        let mut metadata = BTreeMap::new();
        if let Some(id) = self.response_id.take() {
            metadata.insert("response_id".to_owned(), Value::String(id));
        }
        if let Some(model) = self.response_model.take() {
            metadata.insert("response_model".to_owned(), Value::String(model));
        }
        let message = Message {
            role: Role::Assistant,
            content,
            timestamp_ms: 0,
            stop_reason: Some(reason),
            usage: Some(self.usage.clone()),
            metadata,
        };

        if let Some(error) = &self.error_message {
            events.push(StreamEvent::Error {
                reason: StopReason::Error,
                error: error.clone(),
            });
        } else {
            events.push(StreamEvent::Done { reason, message });
        }
    }
}

/// Extracts the data payloads of one SSE text chunk.
fn sse_payloads(data: &str) -> Vec<&str> {
    let mut payloads = Vec::new();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(payload) = line.strip_prefix("data:") {
            payloads.push(payload.strip_prefix(' ').unwrap_or(payload));
        } else if line.starts_with('{') || line.starts_with('[') || !line.contains(':') {
            // A bare JSON payload or the bare `[DONE]` sentinel, with no SSE field name.
            payloads.push(line);
        }
    }
    payloads
}

/// Maps an OpenAI `finish_reason` to a stop reason and an optional error.
fn map_finish_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "tool_calls" | "function_call" => (StopReason::ToolUse, None),
        _ => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {reason}")),
        ),
    }
}

/// Returns whether a usage record contains no token counts at all.
fn is_empty_usage(usage: &Usage) -> bool {
    usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_read_tokens == 0
        && usage.cache_write_tokens == 0
        && usage.total_tokens == 0
}

/// Parses token accounting from a chunk's `usage` object.
fn parse_usage(raw: &Value) -> Usage {
    let prompt = raw
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = raw
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let details = raw.get("prompt_tokens_details");
    let cached = details
        .and_then(|value| value.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| raw.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| raw.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let written = details
        .and_then(|value| value.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input = prompt.saturating_sub(cached).saturating_sub(written);
    let total = raw
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            input
                .saturating_add(completion)
                .saturating_add(cached)
                .saturating_add(written)
        });

    Usage {
        input_tokens: input,
        output_tokens: completion,
        cache_read_tokens: cached,
        cache_write_tokens: written,
        total_tokens: total,
        cost_usd: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: format!("{name} description"),
            parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }
    }

    fn chunk(value: Value) -> String {
        value.to_string()
    }

    #[test]
    fn request_builder_includes_system_tools_and_sampling_options() {
        let cfg = OpenAiRequestConfig {
            max_tokens: Some(512),
            temperature: Some(0.25),
            top_p: Some(0.9),
            reasoning_effort: Some("low".to_owned()),
            tool_choice: Some(json!("auto")),
            ..OpenAiRequestConfig::new("gpt-4o")
        };

        let assistant = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "checking".to_owned(),
                    signature: None,
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({ "path": "Cargo.toml" }),
                }),
            ],
            timestamp_ms: 1,
            stop_reason: Some(StopReason::ToolUse),
            usage: None,
            metadata: BTreeMap::new(),
        };
        let result = Message {
            role: Role::ToolResult,
            content: vec![ContentBlock::ToolResult(navi_types::ToolResult {
                tool_call_id: "call_1".to_owned(),
                tool_name: "read".to_owned(),
                content: vec![ContentBlock::Text {
                    text: "[package]".to_owned(),
                }],
                is_error: false,
                details: None,
            })],
            timestamp_ms: 2,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        };
        let mut messages = vec![Message::user_text("read it", 0), assistant, result];
        messages.insert(
            0,
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "be brief".to_owned(),
                }],
                timestamp_ms: 0,
                stop_reason: None,
                usage: None,
                metadata: BTreeMap::new(),
            },
        );

        let request =
            build_openai_request(&cfg, Some("you are helpful"), &messages, &[tool("read")]);

        assert_eq!(request["model"], json!("gpt-4o"));
        assert_eq!(request["stream"], json!(true));
        assert_eq!(request["stream_options"], json!({ "include_usage": true }));
        assert_eq!(request["max_completion_tokens"], json!(512));
        assert_eq!(request["temperature"], json!(0.25));
        assert_eq!(request["top_p"], json!(0.9));
        assert_eq!(request["reasoning_effort"], json!("low"));
        assert_eq!(request["tool_choice"], json!("auto"));
        let rendered = request["messages"].as_array().expect("messages array");
        assert_eq!(rendered.len(), 5);
        assert_eq!(rendered[0]["role"], json!("system"));
        assert_eq!(rendered[0]["content"], json!("you are helpful"));
        assert_eq!(rendered[1]["content"], json!("be brief"));
        assert_eq!(rendered[2]["content"], json!("read it"));
        assert_eq!(rendered[3]["reasoning_content"], json!("checking"));
        let calls = rendered[3]["tool_calls"].as_array().expect("tool calls");
        assert_eq!(calls[0]["function"]["name"], json!("read"));
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!("{\"path\":\"Cargo.toml\"}")
        );
        assert_eq!(rendered[4]["role"], json!("tool"));
        assert_eq!(rendered[4]["content"], json!("[package]"));
        let tools = request["tools"].as_array().expect("tools array");
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["function"]["name"], json!("read"));
    }

    #[test]
    fn request_builder_handles_images_and_skips_empty_messages() {
        let cfg = OpenAiRequestConfig {
            include_usage: false,
            ..OpenAiRequestConfig::default()
        };
        let user = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "see".to_owned(),
                },
                ContentBlock::Image {
                    data: "AAAA".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ],
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        };
        let empty_assistant = Message {
            role: Role::Assistant,
            content: Vec::new(),
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        };

        let request = build_openai_request(&cfg, Some(""), &[user, empty_assistant], &[]);

        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"][0]["type"], json!("text"));
        assert_eq!(
            messages[0]["content"][1]["image_url"]["url"],
            json!("data:image/png;base64,AAAA")
        );
        assert!(request.get("stream_options").is_none());
        assert!(request.get("tools").is_none());
        assert!(cfg.model.is_empty());
    }

    #[test]
    fn decoder_emits_thinking_and_text_deltas_with_stable_indices() {
        let mut decoder = OpenAiStreamDecoder::new();
        let first = chunk(json!({
            "id": "chat_1",
            "model": "gpt-4o",
            "choices": [{ "index": 0, "delta": { "reasoning_content": "let me " } }]
        }));
        let second = chunk(json!({
            "choices": [{ "index": 0, "delta": { "reasoning_content": "think", "content": "answer" } }]
        }));
        let third = chunk(json!({
            "choices": [{ "index": 0, "delta": { "content": " now" }, "finish_reason": "stop" }]
        }));

        let first_events = decoder.push_chunk(&first).expect("first chunk");
        assert_eq!(
            first_events,
            vec![
                StreamEvent::Start,
                StreamEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "let me ".to_owned(),
                }
            ]
        );
        let second_events = decoder.push_chunk(&second).expect("second chunk");
        assert_eq!(second_events.len(), 2);
        assert_eq!(
            second_events[0],
            StreamEvent::ThinkingDelta {
                content_index: 0,
                delta: "think".to_owned(),
            }
        );
        assert_eq!(
            second_events[1],
            StreamEvent::TextDelta {
                content_index: 1,
                delta: "answer".to_owned(),
            }
        );
        let mut all = decoder.push_chunk(&third).expect("third chunk");
        all.extend(decoder.push_chunk("[DONE]").expect("done"));
        assert_eq!(
            all[0],
            StreamEvent::TextDelta {
                content_index: 1,
                delta: " now".to_owned(),
            }
        );
        let StreamEvent::Done { reason, message } = all[1].clone() else {
            panic!("expected done event");
        };
        assert_eq!(reason, StopReason::Stop);
        assert_eq!(message.text_content(), "answer now");
        assert_eq!(message.content.len(), 2);
        assert_eq!(
            message.content[0],
            ContentBlock::Thinking {
                thinking: "let me think".to_owned(),
                signature: Some("reasoning_content".to_owned()),
            }
        );
        assert_eq!(message.metadata["response_id"], json!("chat_1"));
        assert_eq!(message.metadata["response_model"], json!("gpt-4o"));
    }

    #[test]
    fn decoder_accumulates_tool_arguments_across_chunks() {
        let mut decoder = OpenAiStreamDecoder::new();
        let start = chunk(json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0, "id": "call_7", "type": "function",
                "function": { "name": "read", "arguments": "" }
            }] } }]
        }));
        let middle = chunk(json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0, "function": { "arguments": "{\"path\":" }
            }] } }]
        }));
        let end = chunk(json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0, "function": { "arguments": "\"Cargo.toml\"}" }
            }] }, "finish_reason": "tool_calls" }]
        }));

        let start_events = decoder.push_chunk(&start).expect("start chunk");
        assert_eq!(
            start_events[1],
            StreamEvent::ToolCallStart {
                content_index: 0,
                id: "call_7".to_owned(),
                name: "read".to_owned(),
            }
        );
        let middle_events = decoder.push_chunk(&middle).expect("middle chunk");
        assert_eq!(
            middle_events,
            vec![StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\":".to_owned(),
            }]
        );
        let end_events = decoder.push_chunk(&end).expect("end chunk");
        assert_eq!(
            end_events[0],
            StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "\"Cargo.toml\"}".to_owned(),
            }
        );
        let mut events = decoder.push_chunk("data: [DONE]\n\n").expect("done chunk");
        assert_eq!(
            events[0],
            StreamEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "call_7".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({ "path": "Cargo.toml" }),
                },
            }
        );
        let StreamEvent::Done { reason, message } = events.remove(1) else {
            panic!("expected done event");
        };
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(message.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(message.content.len(), 1);
        assert_eq!(
            message.content[0],
            ContentBlock::ToolCall(ToolCall {
                id: "call_7".to_owned(),
                name: "read".to_owned(),
                arguments: json!({ "path": "Cargo.toml" }),
            })
        );
        assert!(decoder.is_finished());
    }

    #[test]
    fn decoder_extracts_usage_from_chunk_and_choice() {
        let mut decoder = OpenAiStreamDecoder::new();
        let usage_only = chunk(json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": { "cached_tokens": 40, "cache_write_tokens": 10 },
                "total_tokens": 170
            }
        }));
        decoder.push_chunk(&usage_only).expect("usage chunk");
        let finish = chunk(json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "length",
                "usage": { "prompt_tokens": 100, "completion_tokens": 20 } }]
        }));
        decoder.push_chunk(&finish).expect("finish chunk");
        let events = decoder.push_chunk("[DONE]").expect("done chunk");
        let StreamEvent::Done { reason, message } = events.last().cloned().expect("done event")
        else {
            panic!("expected done event");
        };
        assert_eq!(reason, StopReason::Length);
        let usage = message.usage.expect("usage in final message");
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 40);
        assert_eq!(usage.cache_write_tokens, 10);
        assert_eq!(usage.total_tokens, 170);

        // A provider that reports accounting only on the choice still works.
        let mut choice_decoder = OpenAiStreamDecoder::new();
        let choice_usage = chunk(json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop",
                "usage": { "prompt_tokens": 30, "completion_tokens": 4, "prompt_cache_hit_tokens": 12 } }]
        }));
        choice_decoder
            .push_chunk(&choice_usage)
            .expect("choice usage chunk");
        let events = choice_decoder.push_chunk("[DONE]").expect("done chunk");
        let StreamEvent::Done { message, .. } = events.last().cloned().expect("done event") else {
            panic!("expected done event");
        };
        let usage = message.usage.expect("usage in final message");
        assert_eq!(usage.input_tokens, 18);
        assert_eq!(usage.cache_read_tokens, 12);
        assert_eq!(usage.total_tokens, 34);
    }

    #[test]
    fn decoder_maps_stop_length_and_error_finish_reasons() {
        let cases = [
            ("stop", StopReason::Stop),
            ("end", StopReason::Stop),
            ("length", StopReason::Length),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
        ];
        for (raw, expected) in cases {
            let (mapped, error) = map_finish_reason(raw);
            assert_eq!(mapped, expected);
            assert!(error.is_none());
        }
        let (mapped, error) = map_finish_reason("content_filter");
        assert_eq!(mapped, StopReason::Error);
        assert_eq!(
            error.as_deref(),
            Some("Provider finish_reason: content_filter")
        );

        let mut decoder = OpenAiStreamDecoder::new();
        let input = chunk(json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "content_filter" }]
        }));
        decoder.push_chunk(&input).expect("filter chunk");
        let events = decoder.push_chunk("[DONE]").expect("done chunk");
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Error {
                reason: StopReason::Error,
                error: "Provider finish_reason: content_filter".to_owned(),
            })
        );
    }

    #[test]
    fn decoder_finalizes_without_done_and_ignores_later_chunks() {
        let mut decoder = OpenAiStreamDecoder::new();
        let events = decoder.finish().expect("explicit finish");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], StreamEvent::Start);
        let StreamEvent::Done { reason, .. } = events[1].clone() else {
            panic!("expected done event");
        };
        assert_eq!(reason, StopReason::Stop);
        assert!(decoder.is_finished());
        assert!(decoder.finish().expect("second finish").is_empty());
        assert!(decoder.push_chunk("[DONE]").expect("late done").is_empty());
    }

    #[test]
    fn decoder_rejects_malformed_json_as_protocol_error() {
        let mut decoder = OpenAiStreamDecoder::new();
        let error = decoder
            .push_chunk("{not json")
            .expect_err("malformed chunk");
        assert!(matches!(error, NaviError::Protocol(_)));
        assert!(!decoder.is_finished());
        let recovered = decoder
            .push_chunk(&chunk(
                json!({ "choices": [{ "delta": { "content": "ok" } }] }),
            ))
            .expect("valid chunk after error");
        assert_eq!(
            recovered[1],
            StreamEvent::TextDelta {
                content_index: 0,
                delta: "ok".to_owned(),
            }
        );
    }

    #[test]
    fn decoder_handles_multiple_payloads_indexless_calls_and_partial_arguments() {
        let mut decoder = OpenAiStreamDecoder::new();
        let combined = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n\n\
                        : keep-alive\n\
                        event: message\n\
                        data: [DONE]\n\n";
        let events = decoder.push_chunk(combined).expect("combined frame");
        assert_eq!(
            events[1],
            StreamEvent::ToolCallStart {
                content_index: 0,
                id: "c1".to_owned(),
                name: "write".to_owned(),
            }
        );
        assert_eq!(
            events[2],
            StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"x\":1}".to_owned(),
            }
        );
        assert_eq!(
            events[3],
            StreamEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "c1".to_owned(),
                    name: "write".to_owned(),
                    arguments: json!({ "x": 1 }),
                },
            }
        );
        assert_eq!(
            events[4],
            StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "c1".to_owned(),
                        name: "write".to_owned(),
                        arguments: json!({ "x": 1 }),
                    })],
                    timestamp_ms: 0,
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Some(Usage::default()),
                    metadata: BTreeMap::new(),
                },
            }
        );
    }
}
