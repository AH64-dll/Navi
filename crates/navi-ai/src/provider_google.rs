//! Google Gemini Generative Language API request builder and SSE decoder.
//!
//! The builder maps the shared [`navi_types::Message`] representation onto the
//! JSON body of a `generateContent` request: a top-level `systemInstruction`,
//! `contents` with `user`/`model` roles, `functionDeclarations` under `tools`,
//! and sampling settings under `generationConfig`. The decoder consumes the
//! `data:` payloads of a `streamGenerateContent` response, keeps text and
//! thinking blocks separate, resolves `functionCall` parts into tool calls, and
//! turns `usageMetadata` and `finishReason` into the shared stream events.

use navi_types::{
    ContentBlock, Message, NaviError, NaviResult, Role, StopReason, StreamEvent, ToolCall,
    ToolDefinition, ToolResult, Usage,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Request-level settings for [`build_google_request`].
///
/// All fields are optional except the model identifier; the defaults produce a
/// plain request with no sampling overrides, no thinking configuration, and the
/// provider's default function-calling mode.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GoogleRequestConfig {
    /// Model identifier placed at the root of the request parameters.
    pub model: String,
    /// Sampling temperature, sent as `generationConfig.temperature`.
    pub temperature: Option<f64>,
    /// Completion limit, sent as `generationConfig.maxOutputTokens`.
    pub max_tokens: Option<u64>,
    /// Nucleus sampling value, sent as `generationConfig.topP`.
    pub top_p: Option<f64>,
    /// Top-k sampling value, sent as `generationConfig.topK`.
    pub top_k: Option<u64>,
    /// Custom stop sequences, sent as `generationConfig.stopSequences`.
    pub stop_sequences: Vec<String>,
    /// Function calling mode override: `auto`, `none`, or `any`.
    pub tool_choice: Option<String>,
    /// Whether the model should include its thoughts in response parts.
    pub include_thoughts: bool,
    /// Token budget for thinking, where `-1` means dynamic and `0` disables it.
    pub thinking_budget: Option<i64>,
}

impl GoogleRequestConfig {
    /// Creates a request configuration for `model` with all options defaulted.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }
}

/// Builds the JSON body of a Google `generateContent` request.
///
/// The explicit `system` prompt is joined with any `Role::System` messages in
/// the transcript and emitted once as `systemInstruction`. Assistant tool calls
/// become `functionCall` parts and tool results become `functionResponse` parts
/// inside `user` contents; consecutive tool results share one user turn.
pub fn build_google_request(
    cfg: &GoogleRequestConfig,
    system: Option<&str>,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Value {
    let mut body = Map::new();
    if !cfg.model.is_empty() {
        body.insert("model".to_owned(), Value::String(cfg.model.clone()));
    }

    let system_text = join_system_text(system, messages);
    if !system_text.is_empty() {
        body.insert(
            "systemInstruction".to_owned(),
            json!({ "parts": [{ "text": system_text }] }),
        );
    }

    body.insert(
        "contents".to_owned(),
        Value::Array(convert_contents(messages, &cfg.model)),
    );

    if !tools.is_empty() {
        let declarations: Vec<Value> = tools.iter().map(convert_tool).collect();
        body.insert(
            "tools".to_owned(),
            json!([{ "functionDeclarations": declarations }]),
        );
    }

    let mut generation = Map::new();
    if let Some(temperature) = cfg.temperature.filter(|value| value.is_finite()) {
        generation.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(max_tokens) = cfg.max_tokens {
        generation.insert("maxOutputTokens".to_owned(), json!(max_tokens));
    }
    if let Some(top_p) = cfg.top_p.filter(|value| value.is_finite()) {
        generation.insert("topP".to_owned(), json!(top_p));
    }
    if let Some(top_k) = cfg.top_k {
        generation.insert("topK".to_owned(), json!(top_k));
    }
    if !cfg.stop_sequences.is_empty() {
        generation.insert("stopSequences".to_owned(), json!(cfg.stop_sequences));
    }
    if cfg.include_thoughts || cfg.thinking_budget.is_some() {
        let mut thinking = Map::new();
        if cfg.include_thoughts {
            thinking.insert("includeThoughts".to_owned(), json!(true));
        }
        if let Some(budget) = cfg.thinking_budget {
            thinking.insert("thinkingBudget".to_owned(), json!(budget));
        }
        generation.insert("thinkingConfig".to_owned(), Value::Object(thinking));
    }
    if !generation.is_empty() {
        body.insert("generationConfig".to_owned(), Value::Object(generation));
    }

    if let Some(choice) = &cfg.tool_choice {
        let mode = match choice.to_ascii_uppercase().as_str() {
            "NONE" => "NONE",
            "ANY" => "ANY",
            _ => "AUTO",
        };
        body.insert(
            "toolConfig".to_owned(),
            json!({ "functionCallingConfig": { "mode": mode } }),
        );
    }

    Value::Object(body)
}

/// Joins the explicit system prompt with system messages from the transcript.
fn join_system_text(system: Option<&str>, messages: &[Message]) -> String {
    let mut parts = Vec::new();
    if let Some(text) = system.filter(|text| !text.trim().is_empty()) {
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

/// Converts a tool definition into a Gemini function declaration.
fn convert_tool(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

/// Returns whether the model needs explicit function call and response ids.
///
/// Gemini 3 and later require the id to be echoed; older Gemini models and the
/// Claude/GPT models served through Gemini-style endpoints do not accept it.
fn requires_tool_call_id(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude-") || lower.starts_with("gpt-oss-") {
        return true;
    }
    let Some(start) = lower.find("gemini") else {
        return false;
    };
    let suffix = &lower[start + "gemini".len()..];
    let suffix = suffix.strip_prefix("-live").unwrap_or(suffix);
    let digits: String = suffix
        .trim_start_matches('-')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<u32>().is_ok_and(|major| major >= 3)
}

/// Converts all non-system messages into Gemini `Content` objects.
fn convert_contents(messages: &[Message], model: &str) -> Vec<Value> {
    let needs_id = requires_tool_call_id(model);
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());

    for message in messages {
        match message.role {
            Role::System => {}
            Role::User => {
                let parts = user_parts(&message.content, needs_id);
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Role::Assistant => {
                let parts = assistant_parts(&message.content, needs_id);
                if !parts.is_empty() {
                    out.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Role::ToolResult => {
                for block in &message.content {
                    let ContentBlock::ToolResult(result) = block else {
                        continue;
                    };
                    let part = function_response_part(result, needs_id);
                    let mut appended = false;
                    if let Some(last) = out.last_mut()
                        && last.get("role").and_then(Value::as_str) == Some("user")
                        && last
                            .get("parts")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| {
                                parts
                                    .iter()
                                    .any(|part| part.get("functionResponse").is_some())
                            })
                        && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
                    {
                        parts.push(part.clone());
                        appended = true;
                    }
                    if !appended {
                        out.push(json!({ "role": "user", "parts": [part] }));
                    }
                }
            }
        }
    }

    out
}

/// Builds user-side parts: one joined text part followed by inline images.
fn user_parts(content: &[ContentBlock], needs_id: bool) -> Vec<Value> {
    let mut out = Vec::with_capacity(content.len());
    let mut text = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text: value } if !value.is_empty() => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
            ContentBlock::Image { data, mime_type } => {
                if !text.is_empty() {
                    out.push(json!({ "text": text }));
                    text.clear();
                }
                out.push(json!({
                    "inlineData": { "mimeType": mime_type, "data": data },
                }));
            }
            ContentBlock::ToolResult(result) => out.push(function_response_part(result, needs_id)),
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ToolCall(_) => {}
        }
    }
    if !text.is_empty() {
        out.push(json!({ "text": text }));
    }
    out
}

/// Builds model-side parts: text, thinking, and function calls in order.
fn assistant_parts(content: &[ContentBlock], needs_id: bool) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(json!({ "text": text })),
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                Some(json!({ "thought": true, "text": thinking }))
            }
            ContentBlock::ToolCall(call) => {
                let mut function_call = Map::new();
                function_call.insert("name".to_owned(), Value::String(call.name.clone()));
                function_call.insert("args".to_owned(), call.arguments.clone());
                if needs_id {
                    function_call.insert("id".to_owned(), Value::String(call.id.clone()));
                }
                Some(json!({ "functionCall": Value::Object(function_call) }))
            }
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::ToolResult(_) => None,
        })
        .collect()
}

/// Converts a shared tool result into a Gemini `functionResponse` part.
fn function_response_part(result: &ToolResult, needs_id: bool) -> Value {
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let response = if result.is_error {
        json!({
            "error": if text.is_empty() { "(no tool output)" } else { text.as_str() },
        })
    } else {
        json!({ "output": text })
    };

    let mut function_response = Map::new();
    function_response.insert("name".to_owned(), Value::String(result.tool_name.clone()));
    function_response.insert("response".to_owned(), response);
    if needs_id {
        function_response.insert("id".to_owned(), Value::String(result.tool_call_id.clone()));
    }
    let images: Vec<Value> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data, mime_type } => Some(json!({
                "inlineData": { "mimeType": mime_type, "data": data },
            })),
            _ => None,
        })
        .collect();
    if !images.is_empty() {
        function_response.insert("parts".to_owned(), Value::Array(images));
    }

    json!({ "functionResponse": Value::Object(function_response) })
}

/// Maps a Gemini `finishReason` onto the shared stop reason.
fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        // The continuation token is not resumed, so it ends the turn as length.
        "MAX_TOKENS" | "CONTINUATION" => StopReason::Length,
        "TOOL_USE" => StopReason::ToolUse,
        // SAFETY, blocklist, recitation, and unknown reasons are surfaced as errors.
        _ => StopReason::Error,
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
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            if !payload.is_empty() {
                payloads.push(payload);
            }
        } else if line.starts_with('{') || line.starts_with('[') || !line.contains(':') {
            // A bare JSON payload, with no SSE field name.
            payloads.push(line);
        }
    }
    payloads
}

/// Incremental decoder for Google `streamGenerateContent` SSE payloads.
///
/// Feed each `data:` line (or a bare JSON payload) to [`Self::push_chunk`].
/// Text and thought parts are accumulated in separate content blocks; a
/// `functionCall` part emits its start, argument delta, and end events at once
/// because Gemini sends the call as a complete object. Call [`Self::finish`]
/// once the HTTP stream ends to obtain the terminal `Done` or `Error` event.
#[derive(Debug, Default)]
pub struct GoogleStreamDecoder {
    started: bool,
    finished: bool,
    next_index: usize,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    blocks: Vec<Option<ContentBlock>>,
    tool_ids: BTreeSet<String>,
    tool_counter: u64,
    stop_reason: Option<StopReason>,
    error_message: Option<String>,
    usage: Usage,
    response_id: Option<String>,
    response_model: Option<String>,
}

impl GoogleStreamDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns whether [`Self::finish`] already closed the stream.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Consumes one chunk of SSE text and returns the events it produced.
    ///
    /// Both bare JSON payloads and lines prefixed with `data:` are accepted.
    /// Malformed JSON is reported as [`NaviError::Protocol`] without closing
    /// the decoder, so a later valid chunk can still be decoded.
    pub fn push_chunk(&mut self, data: &str) -> NaviResult<Vec<StreamEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();

        // A complete JSON document may span several physical lines, in which
        // case the line-based SSE splitter below would break it apart.
        let trimmed = data.trim();
        if trimmed.starts_with('{')
            && let Ok(chunk) = serde_json::from_str::<Value>(trimmed)
        {
            self.consume_chunk(&chunk, &mut events);
            return Ok(events);
        }

        for payload in sse_payloads(data) {
            if payload.trim() == "[DONE]" {
                continue;
            }
            let chunk: Value = serde_json::from_str(payload).map_err(|error| {
                NaviError::Protocol(format!("invalid Google stream chunk: {error}"))
            })?;
            self.consume_chunk(&chunk, &mut events);
        }
        Ok(events)
    }

    /// Closes the stream and returns the terminal events.
    ///
    /// A stream with no explicit finish reason, or with Gemini's `STOP`, ends
    /// as `ToolUse` when it contained a tool call and as `Stop` otherwise.
    /// Calling this after completion is a no-op.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut events = Vec::new();
        self.emit_start(&mut events);
        let content: Vec<ContentBlock> = self.blocks.iter().flatten().cloned().collect();
        let has_tools = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        // Gemini reports `STOP` even when the turn ends with function calls.
        // Match the reference adapter: an otherwise plain stop becomes ToolUse.
        let reason = if has_tools && matches!(self.stop_reason, None | Some(StopReason::Stop)) {
            StopReason::ToolUse
        } else {
            self.stop_reason.unwrap_or(StopReason::Stop)
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

        if let Some(error) = self.error_message.take() {
            events.push(StreamEvent::Error {
                reason: StopReason::Error,
                error,
            });
        } else {
            events.push(StreamEvent::Done { reason, message });
        }
        events
    }

    /// Emits the single `Start` event for a stream.
    fn emit_start(&mut self, events: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            events.push(StreamEvent::Start);
        }
    }

    /// Applies one parsed response chunk to the decoder state.
    fn consume_chunk(&mut self, chunk: &Value, events: &mut Vec<StreamEvent>) {
        if let Some(id) = chunk.get("responseId").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(model) = chunk.get("modelVersion").and_then(Value::as_str) {
            self.response_model = Some(model.to_owned());
        }
        if let Some(usage) = chunk.get("usageMetadata") {
            self.apply_usage(usage);
        }
        if let Some(feedback) = chunk.get("promptFeedback")
            && let Some(reason) = feedback.get("blockReason").and_then(Value::as_str)
        {
            let detail = feedback
                .get("blockReasonMessage")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty());
            let error = match detail {
                Some(detail) => format!("Google prompt blocked: {reason}. {detail}"),
                None => format!("Google prompt blocked: {reason}"),
            };
            if self.error_message.is_none() {
                self.error_message = Some(error);
            }
        }

        let Some(candidates) = chunk.get("candidates").and_then(Value::as_array) else {
            return;
        };
        for candidate in candidates {
            if let Some(content) = candidate.get("content") {
                self.consume_content(content, events);
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                let mapped = map_finish_reason(reason);
                self.stop_reason = Some(mapped);
                if mapped == StopReason::Error && self.error_message.is_none() {
                    self.error_message = Some(format!("Provider finish_reason: {reason}"));
                }
            }
        }
    }

    /// Processes the parts of one candidate's content.
    ///
    /// `Start` is emitted only when a part produces content. Chunks that carry
    /// only metadata (usage, prompt feedback, or a finish reason) leave the
    /// stream unstarted, so [`Self::finish`] can emit `Start` ahead of the
    /// terminal event for an empty or blocked stream.
    fn consume_content(&mut self, content: &Value, events: &mut Vec<StreamEvent>) {
        let Some(parts) = content.get("parts").and_then(Value::as_array) else {
            return;
        };
        for part in parts {
            let is_thinking = part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                self.emit_start(events);
                let index = self.ensure_text_block(is_thinking);
                self.append_text(index, is_thinking, text, part);
                if is_thinking {
                    events.push(StreamEvent::ThinkingDelta {
                        content_index: index,
                        delta: text.to_owned(),
                    });
                } else {
                    events.push(StreamEvent::TextDelta {
                        content_index: index,
                        delta: text.to_owned(),
                    });
                }
            }
            if let Some(call) = part.get("functionCall") {
                self.emit_start(events);
                self.consume_function_call(call, events);
            }
        }
    }

    /// Returns the index of the current text or thinking block, creating it once.
    fn ensure_text_block(&mut self, thinking: bool) -> usize {
        let existing = if thinking {
            self.thinking_index
        } else {
            self.text_index
        };
        if let Some(index) = existing {
            return index;
        }
        let index = self.next_index;
        self.next_index += 1;
        let block = if thinking {
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
            }
        } else {
            ContentBlock::Text {
                text: String::new(),
            }
        };
        self.blocks.push(Some(block));
        if thinking {
            self.thinking_index = Some(index);
        } else {
            self.text_index = Some(index);
        }
        index
    }

    /// Appends a text delta, retaining a thought signature when present.
    fn append_text(&mut self, index: usize, thinking: bool, text: &str, part: &Value) {
        let signature = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        if let Some(slot) = self.blocks.get_mut(index).and_then(Option::as_mut) {
            match slot {
                ContentBlock::Text { text: target } if !thinking => target.push_str(text),
                ContentBlock::Thinking {
                    thinking: target,
                    signature: target_signature,
                } if thinking => {
                    target.push_str(text);
                    if let Some(signature) = signature {
                        *target_signature = Some(signature.to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    /// Emits the three tool-call events for a complete `functionCall` part.
    fn consume_function_call(&mut self, call: &Value, events: &mut Vec<StreamEvent>) {
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = call
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && !self.tool_ids.contains(*id))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                self.tool_counter += 1;
                loop {
                    let candidate = format!("{name}_{}", self.tool_counter);
                    if !self.tool_ids.contains(&candidate) {
                        return candidate;
                    }
                    self.tool_counter += 1;
                }
            });
        self.tool_ids.insert(id.clone());

        let tool_call = ToolCall {
            id,
            name: name.to_owned(),
            arguments,
        };
        let index = self.next_index;
        self.next_index += 1;
        self.blocks
            .push(Some(ContentBlock::ToolCall(tool_call.clone())));

        events.push(StreamEvent::ToolCallStart {
            content_index: index,
            id: tool_call.id.clone(),
            name: tool_call.name.clone(),
        });
        let delta = serde_json::to_string(&tool_call.arguments).unwrap_or_else(|_| "{}".to_owned());
        events.push(StreamEvent::ToolCallDelta {
            content_index: index,
            delta,
        });
        events.push(StreamEvent::ToolCallEnd {
            content_index: index,
            tool_call,
        });
    }

    /// Applies `usageMetadata` from a response chunk.
    fn apply_usage(&mut self, raw: &Value) {
        let prompt = raw
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = raw
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let candidates = raw
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let thoughts = raw
            .get("thoughtsTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = raw
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                prompt
                    .saturating_add(candidates)
                    .saturating_add(thoughts)
                    .saturating_add(cached)
            });
        self.usage = Usage {
            input_tokens: prompt.saturating_sub(cached),
            output_tokens: candidates.saturating_add(thoughts),
            cache_read_tokens: cached,
            cache_write_tokens: 0,
            total_tokens: total,
            cost_usd: 0.0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as TestMap;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: format!("{name} description"),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }
    }

    fn tool_result(id: &str, name: &str, text: &str, is_error: bool) -> Message {
        Message {
            role: Role::ToolResult,
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_call_id: id.to_owned(),
                tool_name: name.to_owned(),
                content: vec![ContentBlock::Text {
                    text: text.to_owned(),
                }],
                is_error,
                details: None,
            })],
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: TestMap::new(),
        }
    }

    #[test]
    fn build_request_includes_system_instruction_and_user_contents() {
        let cfg = GoogleRequestConfig::new("gemini-2.5-flash");
        let messages = vec![
            Message::user_text("hello", 0),
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "from transcript".to_owned(),
                }],
                timestamp_ms: 0,
                stop_reason: None,
                usage: None,
                metadata: TestMap::new(),
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "look".to_owned(),
                    },
                    ContentBlock::Image {
                        data: "YWJj".to_owned(),
                        mime_type: "image/png".to_owned(),
                    },
                ],
                timestamp_ms: 0,
                stop_reason: None,
                usage: None,
                metadata: TestMap::new(),
            },
        ];

        let request = build_google_request(&cfg, Some("be concise"), &messages, &[]);

        assert_eq!(request["model"], json!("gemini-2.5-flash"));
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            json!("be concise\n\nfrom transcript")
        );
        let contents = request["contents"]
            .as_array()
            .expect("contents is an array");
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], json!("user"));
        assert_eq!(contents[0]["parts"][0]["text"], json!("hello"));
        assert_eq!(contents[1]["role"], json!("user"));
        assert_eq!(contents[1]["parts"][0]["text"], json!("look"));
        assert_eq!(
            contents[1]["parts"][1]["inlineData"],
            json!({ "mimeType": "image/png", "data": "YWJj" })
        );
        assert!(request.get("tools").is_none());
        assert!(request.get("generationConfig").is_none());
    }

    #[test]
    fn build_request_converts_model_parts_tools_and_options() {
        let cfg = GoogleRequestConfig {
            temperature: Some(0.2),
            max_tokens: Some(512),
            top_p: Some(0.9),
            top_k: Some(40),
            stop_sequences: vec!["END".to_owned()],
            tool_choice: Some("any".to_owned()),
            include_thoughts: true,
            thinking_budget: Some(-1),
            ..GoogleRequestConfig::new("gemini-3-pro-preview")
        };
        let assistant = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "step one".to_owned(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "checking".to_owned(),
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
            metadata: TestMap::new(),
        };
        let messages = vec![assistant, tool_result("call_1", "read", "[package]", false)];

        let request = build_google_request(&cfg, None, &messages, &[tool("read")]);

        assert!(request.get("systemInstruction").is_none());
        assert_eq!(
            request["generationConfig"],
            json!({
                "temperature": 0.2,
                "maxOutputTokens": 512,
                "topP": 0.9,
                "topK": 40,
                "stopSequences": ["END"],
                "thinkingConfig": { "includeThoughts": true, "thinkingBudget": -1 },
            })
        );
        assert_eq!(
            request["toolConfig"],
            json!({ "functionCallingConfig": { "mode": "ANY" } })
        );
        assert_eq!(
            request["tools"][0]["functionDeclarations"][0]["name"],
            json!("read")
        );
        assert_eq!(
            request["tools"][0]["functionDeclarations"][0]["parameters"]["required"],
            json!(["path"])
        );
        let contents = request["contents"]
            .as_array()
            .expect("contents is an array");
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], json!("model"));
        assert_eq!(contents[0]["parts"][0]["thought"], json!(true));
        assert_eq!(contents[0]["parts"][1]["text"], json!("checking"));
        assert_eq!(
            contents[0]["parts"][2]["functionCall"]["name"],
            json!("read")
        );
        assert_eq!(
            contents[0]["parts"][2]["functionCall"]["args"],
            json!({ "path": "Cargo.toml" })
        );
        assert_eq!(
            contents[0]["parts"][2]["functionCall"]["id"],
            json!("call_1")
        );
        assert_eq!(contents[1]["role"], json!("user"));
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"],
            json!({
                "name": "read",
                "response": { "output": "[package]" },
                "id": "call_1",
            })
        );
    }

    #[test]
    fn build_request_merges_error_results_and_skips_empty_messages() {
        let cfg = GoogleRequestConfig::default();
        let empty = Message {
            role: Role::Assistant,
            content: Vec::new(),
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: TestMap::new(),
        };
        let first = tool_result("c1", "a", "bad", true);
        let second = tool_result("c2", "b", "", true);
        let request = build_google_request(&cfg, Some(""), &[empty, first, second], &[]);

        assert!(request.get("model").is_none());
        assert!(request.get("tools").is_none());
        assert!(request.get("systemInstruction").is_none());
        let contents = request["contents"]
            .as_array()
            .expect("contents is an array");
        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().expect("parts is an array");
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionResponse"]["response"],
            json!({ "error": "bad" })
        );
        assert_eq!(
            parts[1]["functionResponse"]["response"],
            json!({ "error": "(no tool output)" })
        );
    }

    #[test]
    fn decoder_streams_text_thinking_and_usage() {
        let mut decoder = GoogleStreamDecoder::new();
        let first = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]}}],"responseId":"r1","modelVersion":"gemini-2.5-flash"}"#;
        let second = r#"data: {"candidates":[{"content":{"parts":[{"thought":true,"text":"reason","thoughtSignature":"sig"},{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":2,"candidatesTokenCount":5,"thoughtsTokenCount":3,"totalTokenCount":18}}"#;

        let first_events = decoder.push_chunk(first).expect("first chunk");
        assert_eq!(
            first_events,
            vec![
                StreamEvent::Start,
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "Hello".to_owned(),
                },
            ]
        );
        let events = decoder.push_chunk(second).expect("second chunk");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            StreamEvent::ThinkingDelta {
                content_index: 1,
                delta: "reason".to_owned(),
            }
        );
        assert_eq!(
            events[1],
            StreamEvent::TextDelta {
                content_index: 0,
                delta: " world".to_owned(),
            }
        );

        let terminal = decoder.finish();
        assert_eq!(terminal.len(), 1);
        let StreamEvent::Done { reason, message } = &terminal[0] else {
            panic!("expected done event");
        };
        assert_eq!(*reason, StopReason::Stop);
        assert_eq!(message.content.len(), 2);
        assert_eq!(message.text_content(), "Hello world");
        assert_eq!(
            message.content[1],
            ContentBlock::Thinking {
                thinking: "reason".to_owned(),
                signature: Some("sig".to_owned()),
            }
        );
        let usage = message.usage.as_ref().expect("usage is present");
        assert_eq!(usage.input_tokens, 8);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cache_read_tokens, 2);
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(message.metadata["response_id"], json!("r1"));
        assert_eq!(
            message.metadata["response_model"],
            json!("gemini-2.5-flash")
        );
        assert!(decoder.is_finished());
        assert!(decoder.finish().is_empty());
        assert!(decoder.push_chunk(first).expect("late chunk").is_empty());
    }

    #[test]
    fn decoder_emits_function_call_events_and_tool_use_reason() {
        let mut decoder = GoogleStreamDecoder::new();
        let chunk = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"Cargo.toml"}}}]},"finishReason":"STOP"}]}"#;
        let events = decoder.push_chunk(chunk).expect("function call chunk");

        assert_eq!(events.len(), 4);
        assert_eq!(events[0], StreamEvent::Start);
        let StreamEvent::ToolCallStart {
            content_index,
            id,
            name,
        } = &events[1]
        else {
            panic!("expected tool call start");
        };
        assert_eq!(*content_index, 0);
        assert_eq!(name, "read");
        assert_eq!(id, "read_1");
        assert_eq!(
            events[2],
            StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\":\"Cargo.toml\"}".to_owned(),
            }
        );
        assert_eq!(
            events[3],
            StreamEvent::ToolCallEnd {
                content_index: 0,
                tool_call: ToolCall {
                    id: "read_1".to_owned(),
                    name: "read".to_owned(),
                    arguments: json!({ "path": "Cargo.toml" }),
                },
            }
        );

        let terminal = decoder.finish();
        let StreamEvent::Done { reason, message } = &terminal[0] else {
            panic!("expected done event");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(message.content.len(), 1);
    }

    #[test]
    fn decoder_preserves_provided_tool_ids_and_generates_unique_ones() {
        let mut decoder = GoogleStreamDecoder::new();
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [
                    { "functionCall": { "id": "call_1", "name": "read", "args": {} } },
                    { "functionCall": { "id": "call_1", "name": "write", "args": {} } },
                    { "functionCall": { "name": "list", "args": {} } }
                ] },
                "finishReason": "STOP"
            }]
        })
        .to_string();
        let events = decoder.push_chunk(&chunk).expect("tool call chunk");
        let starts: Vec<&StreamEvent> = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 3);
        let StreamEvent::ToolCallStart { id, .. } = starts[0] else {
            unreachable!()
        };
        assert_eq!(id, "call_1");
        let StreamEvent::ToolCallStart { id, .. } = starts[1] else {
            unreachable!()
        };
        assert_eq!(id, "write_1");
        let StreamEvent::ToolCallStart { id, .. } = starts[2] else {
            unreachable!()
        };
        assert_eq!(id, "list_2");
    }

    #[test]
    fn decoder_maps_stop_length_and_safety_finish_reasons() {
        for (raw, expected) in [
            ("STOP", StopReason::Stop),
            ("MAX_TOKENS", StopReason::Length),
            ("CONTINUATION", StopReason::Length),
        ] {
            let mut decoder = GoogleStreamDecoder::new();
            let chunk = format!(
                r#"data: {{"candidates":[{{"content":{{"parts":[{{"text":"ok"}}]}},"finishReason":"{raw}"}}]}}"#
            );
            let _ = decoder.push_chunk(&chunk).expect("chunk");
            let terminal = decoder.finish();
            let StreamEvent::Done { reason, .. } = &terminal[0] else {
                panic!("expected done event for {raw}");
            };
            assert_eq!(*reason, expected, "raw reason {raw}");
        }

        let mut decoder = GoogleStreamDecoder::new();
        let chunk = r#"data: {"candidates":[{"finishReason":"SAFETY"}]}"#;
        let _ = decoder.push_chunk(chunk).expect("safety chunk");
        assert_eq!(
            decoder.finish(),
            vec![
                StreamEvent::Start,
                StreamEvent::Error {
                    reason: StopReason::Error,
                    error: "Provider finish_reason: SAFETY".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn decoder_reports_prompt_block_and_malformed_json() {
        let mut decoder = GoogleStreamDecoder::new();
        let error = decoder
            .push_chunk("{not json")
            .expect_err("malformed chunk is rejected");
        assert!(matches!(error, NaviError::Protocol(_)));
        assert!(!decoder.is_finished());

        let recovered = decoder
            .push_chunk(r#"data: {"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#)
            .expect("valid chunk after error");
        assert_eq!(
            recovered,
            vec![
                StreamEvent::Start,
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "ok".to_owned(),
                },
            ]
        );

        let mut blocked = GoogleStreamDecoder::new();
        let feedback =
            r#"data: {"promptFeedback":{"blockReason":"SAFETY","blockReasonMessage":"blocked"}}"#;
        let _ = blocked.push_chunk(feedback).expect("feedback chunk");
        assert_eq!(
            blocked.finish(),
            vec![
                StreamEvent::Start,
                StreamEvent::Error {
                    reason: StopReason::Error,
                    error: "Google prompt blocked: SAFETY. blocked".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn decoder_handles_multiple_frames_bare_json_and_pretty_json() {
        let mut decoder = GoogleStreamDecoder::new();
        let combined = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\n\
                        : keep-alive\n\
                        event: message\n\
                        data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"b\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":1,\"totalTokenCount\":5}}\n\n";
        let events = decoder.push_chunk(combined).expect("combined frames");
        assert_eq!(
            events,
            vec![
                StreamEvent::Start,
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "a".to_owned(),
                },
                StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "b".to_owned(),
                },
            ]
        );
        let terminal = decoder.finish();
        let StreamEvent::Done { message, .. } = &terminal[0] else {
            panic!("expected done event");
        };
        assert_eq!(message.text_content(), "ab");
        assert_eq!(
            message.usage.as_ref().expect("usage"),
            &Usage {
                input_tokens: 4,
                output_tokens: 1,
                total_tokens: 5,
                ..Usage::default()
            }
        );

        let mut pretty = GoogleStreamDecoder::new();
        let pretty_chunk = serde_json::to_string_pretty(&json!({
            "candidates": [{ "content": { "parts": [{ "text": "pretty" }] }, "finishReason": "STOP" }]
        }))
        .expect("pretty json");
        let events = pretty.push_chunk(&pretty_chunk).expect("pretty chunk");
        assert_eq!(
            events[1],
            StreamEvent::TextDelta {
                content_index: 0,
                delta: "pretty".to_owned(),
            }
        );
    }

    #[test]
    fn finish_on_empty_stream_emits_start_and_done() {
        let mut decoder = GoogleStreamDecoder::default();
        assert_eq!(
            decoder.finish(),
            vec![
                StreamEvent::Start,
                StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: Message {
                        role: Role::Assistant,
                        content: Vec::new(),
                        timestamp_ms: 0,
                        stop_reason: Some(StopReason::Stop),
                        usage: Some(Usage::default()),
                        metadata: TestMap::new(),
                    },
                },
            ]
        );
    }

    #[test]
    fn build_request_drops_non_finite_sampling_and_normalizes_tool_choice() {
        let cfg = GoogleRequestConfig {
            temperature: Some(f64::NAN),
            top_p: Some(f64::INFINITY),
            tool_choice: Some("unsupported".to_owned()),
            ..GoogleRequestConfig::default()
        };
        let request = build_google_request(&cfg, None, &[], &[]);

        assert!(request.get("model").is_none());
        assert!(request.get("generationConfig").is_none());
        assert_eq!(request["contents"], json!([]));
        assert_eq!(
            request["toolConfig"],
            json!({ "functionCallingConfig": { "mode": "AUTO" } })
        );

        let none = GoogleRequestConfig {
            tool_choice: Some("none".to_owned()),
            ..GoogleRequestConfig::default()
        };
        let request = build_google_request(&none, None, &[], &[]);
        assert_eq!(
            request["toolConfig"]["functionCallingConfig"]["mode"],
            json!("NONE")
        );
    }
}
