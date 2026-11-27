//! Synchronous multi-provider HTTP/SSE client for LLM endpoints.
//!
//! The client owns a `ureq` agent and one endpoint configuration. Both
//! [`LlmHttpClient::complete`] and [`LlmHttpClient::complete_streaming`] issue
//! a streaming request; the former simply discards the emitted events and
//! returns the final assistant message. Provider wire formats are produced by
//! the request builders in the sibling provider modules and decoded by their
//! incremental stream decoders, so this module only handles transport, retries,
//! SSE framing, and the uniform event callback.

use crate::error_body::{format_provider_error, parse_provider_error_body};
use crate::provider_anthropic::{
    AnthropicRequestConfig, AnthropicStreamDecoder, build_anthropic_request,
};
use crate::provider_google::{GoogleRequestConfig, GoogleStreamDecoder, build_google_request};
use crate::provider_openai::{OpenAiRequestConfig, OpenAiStreamDecoder, build_openai_request};
use crate::retry::{RetryDecision, RetryPolicy, decide_retry, parse_retry_after_ms};
use crate::sse::{SseEvent, SseParser, is_done_event};
use navi_types::{Message, NaviError, NaviResult, StreamEvent, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Wire protocol used by an LLM endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI Chat Completions, including gateway-compatible variants.
    #[default]
    #[serde(rename = "openai_compat")]
    OpenAiCompat,
    /// Anthropic Messages API.
    Anthropic,
    /// Google Gemini Generative Language API.
    Google,
}

/// Connection and generation settings for one LLM endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmEndpointConfig {
    /// Provider wire protocol to use.
    pub kind: ProviderKind,
    /// Base URL, usually including the API version (for example `.../v1`).
    pub base_url: String,
    /// API key sent in the provider-specific authentication header.
    pub api_key: String,
    /// Model identifier sent in the request body.
    pub model: String,
    /// Upper bound on generated tokens. Zero lets the provider choose.
    pub max_tokens: u64,
    /// Per-request timeout in seconds. Zero is treated as one second.
    pub timeout_secs: u64,
    /// Retry policy applied to transient transport and HTTP failures.
    #[serde(default)]
    pub retry_policy: RetryPolicy,
}

impl LlmEndpointConfig {
    /// Builds a config with default token, timeout, and retry settings.
    pub fn new(
        kind: ProviderKind,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            ..Self::default()
        }
    }
}

impl Default for LlmEndpointConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::default(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_tokens: 4096,
            timeout_secs: 120,
            retry_policy: RetryPolicy::default(),
        }
    }
}

/// Synchronous client that speaks the three supported LLM HTTP protocols.
#[derive(Debug, Clone)]
pub struct LlmHttpClient {
    cfg: LlmEndpointConfig,
    agent: ureq::Agent,
}

impl LlmHttpClient {
    /// Creates a client for `cfg` with a shared, timeout-configured agent.
    pub fn new(cfg: LlmEndpointConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
            .build();
        Self { cfg, agent }
    }

    /// Returns the endpoint configuration this client was built with.
    pub fn config(&self) -> &LlmEndpointConfig {
        &self.cfg
    }

    /// Completes one turn and returns the assistant message.
    pub fn complete(
        &self,
        system: Option<&str>,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> NaviResult<Message> {
        self.complete_streaming(system, messages, tools, &mut |_| {})
    }

    /// Completes one turn, forwarding every stream event to `on_event`.
    ///
    /// The returned message is the one carried by the terminal `Done` event. A
    /// provider-level error, whether reported as an HTTP error, an SSE `error`
    /// event, or an error stop reason, is returned as [`NaviError::Provider`].
    pub fn complete_streaming(
        &self,
        system: Option<&str>,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_event: &mut dyn FnMut(&StreamEvent),
    ) -> NaviResult<Message> {
        self.validate()?;
        let url = self.endpoint_url()?;
        let body = self.request_body(system, messages, tools);
        let response = self.send_with_retries(&url, &body)?;
        self.consume_response(response, on_event)
    }

    /// Rejects configs that cannot produce a usable request.
    fn validate(&self) -> NaviResult<()> {
        if self.cfg.base_url.trim().is_empty() {
            return Err(NaviError::InvalidInput(
                "LLM endpoint base_url must not be empty".to_owned(),
            ));
        }
        if self.cfg.model.trim().is_empty() {
            return Err(NaviError::InvalidInput(
                "LLM endpoint model must not be empty".to_owned(),
            ));
        }
        Ok(())
    }

    /// Resolves the streaming endpoint URL from the base URL and model.
    fn endpoint_url(&self) -> NaviResult<String> {
        let base = self.cfg.base_url.trim_end_matches('/');
        match self.cfg.kind {
            ProviderKind::OpenAiCompat => {
                if base.ends_with("/chat/completions") {
                    Ok(base.to_owned())
                } else {
                    Ok(format!("{base}/chat/completions"))
                }
            }
            ProviderKind::Anthropic => {
                if base.ends_with("/v1/messages") {
                    Ok(base.to_owned())
                } else {
                    let root = base.strip_suffix("/v1").unwrap_or(base);
                    Ok(format!("{root}/v1/messages"))
                }
            }
            ProviderKind::Google => {
                let model = self
                    .cfg
                    .model
                    .strip_prefix("models/")
                    .unwrap_or(&self.cfg.model);
                Ok(format!(
                    "{base}/models/{}:streamGenerateContent?alt=sse",
                    url_path_segment(model)
                ))
            }
        }
    }

    /// Builds the provider-specific streaming request body.
    fn request_body(
        &self,
        system: Option<&str>,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Value {
        let model = self.cfg.model.as_str();
        match self.cfg.kind {
            ProviderKind::OpenAiCompat => {
                let mut cfg = OpenAiRequestConfig::new(model);
                cfg.max_tokens = (self.cfg.max_tokens > 0).then_some(self.cfg.max_tokens);
                build_openai_request(&cfg, system, messages, tools)
            }
            ProviderKind::Anthropic => {
                let cfg = AnthropicRequestConfig::new(model, self.cfg.max_tokens.max(1));
                build_anthropic_request(&cfg, system, messages, tools)
            }
            ProviderKind::Google => {
                let mut cfg = GoogleRequestConfig::new(model);
                cfg.max_tokens = (self.cfg.max_tokens > 0).then_some(self.cfg.max_tokens);
                build_google_request(&cfg, system, messages, tools)
            }
        }
    }

    /// Sends the request, retrying transient failures under the configured policy.
    fn send_with_retries(&self, url: &str, body: &Value) -> NaviResult<ureq::Response> {
        let policy = self.cfg.retry_policy;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let request = self
                .agent
                .post(url)
                .set("Content-Type", "application/json")
                .set("Accept", "text/event-stream, application/json");
            let request = match self.cfg.kind {
                ProviderKind::OpenAiCompat => {
                    request.set("Authorization", &format!("Bearer {}", self.cfg.api_key))
                }
                ProviderKind::Anthropic => request
                    .set("x-api-key", &self.cfg.api_key)
                    .set("anthropic-version", "2023-06-01"),
                ProviderKind::Google => request.set("x-goog-api-key", &self.cfg.api_key),
            };

            let payload = body.to_string();
            match request.send_string(&payload) {
                Ok(response) => return Ok(response),
                Err(ureq::Error::Status(status, response)) => {
                    let retry_after = response
                        .header("Retry-After")
                        .and_then(parse_retry_after_ms);
                    let raw = response.into_string().unwrap_or_default();
                    let parsed = parse_provider_error_body(status, &raw);
                    let message = format_provider_error(&parsed);
                    let error = NaviError::Provider(message.clone());
                    if !self.retry_after_failure(
                        attempt,
                        policy,
                        Some(status),
                        &message,
                        retry_after,
                    ) {
                        return Err(error);
                    }
                }
                Err(ureq::Error::Transport(transport)) => {
                    let message = transport.to_string();
                    let error = NaviError::Transport(message.clone());
                    if !self.retry_after_failure(attempt, policy, None, &message, None) {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Applies one retry decision, sleeping when another request is warranted.
    fn retry_after_failure(
        &self,
        attempt: u32,
        policy: RetryPolicy,
        status: Option<u16>,
        message: &str,
        retry_after_ms: Option<u64>,
    ) -> bool {
        let seed = retry_seed(attempt);
        match decide_retry(&policy, attempt, status, message, retry_after_ms, seed) {
            RetryDecision::Retry { delay_ms, .. } => {
                if delay_ms > 0 {
                    thread::sleep(Duration::from_millis(delay_ms));
                }
                true
            }
            RetryDecision::Exhausted | RetryDecision::NotRetryable => false,
        }
    }

    /// Reads an HTTP response as SSE and produces events plus the final message.
    fn consume_response(
        &self,
        response: ureq::Response,
        on_event: &mut dyn FnMut(&StreamEvent),
    ) -> NaviResult<Message> {
        let mut decoder = Decoder::new(self.cfg.kind);
        let mut parser = SseParser::new();
        let mut frames: Vec<SseEvent> = Vec::new();
        let mut raw = String::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut reader = response.into_reader();
        let mut buf = [0u8; 8192];

        loop {
            let count = reader
                .read(&mut buf)
                .map_err(|error| NaviError::Transport(error.to_string()))?;
            if count == 0 {
                break;
            }
            pending.extend_from_slice(&buf[..count]);
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    let text = text.to_owned();
                    pending.clear();
                    push_text(&mut parser, &mut raw, &mut frames, &text);
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = String::from_utf8_lossy(&pending[..valid]);
                        push_text(&mut parser, &mut raw, &mut frames, &text);
                        pending.drain(..valid);
                    }
                }
            }
        }
        if !pending.is_empty() {
            let text = String::from_utf8_lossy(&pending);
            push_text(&mut parser, &mut raw, &mut frames, &text);
        }
        frames.extend(parser.finish());

        // Some gateways ignore `stream: true` and answer with a plain JSON
        // object. Translate that shape into decoder frames instead of failing.
        if frames.is_empty() && raw.trim_start().starts_with('{') {
            return decode_plain_json(self.cfg.kind, raw.trim(), on_event);
        }

        let mut started = false;
        let mut final_message = None;
        for frame in frames {
            let events = decoder.push(&frame)?;
            emit_events(events, on_event, &mut started, &mut final_message)?;
        }
        let tail = decoder.finish()?;
        if !started && !tail.is_empty() {
            on_event(&StreamEvent::Start);
        }
        emit_events(tail, on_event, &mut started, &mut final_message)?;
        final_message.ok_or_else(|| {
            NaviError::Protocol("LLM stream ended without a terminal message".to_owned())
        })
    }
}

/// Provider decoder selected by the endpoint configuration.
enum Decoder {
    OpenAI(OpenAiStreamDecoder),
    Anthropic(AnthropicStreamDecoder),
    Google(GoogleStreamDecoder),
}

impl Decoder {
    /// Creates the decoder matching `kind`.
    fn new(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenAiCompat => Self::OpenAI(OpenAiStreamDecoder::new()),
            ProviderKind::Anthropic => Self::Anthropic(AnthropicStreamDecoder::new()),
            ProviderKind::Google => Self::Google(GoogleStreamDecoder::new()),
        }
    }

    /// Applies one SSE frame and returns the shared events it produced.
    fn push(&mut self, frame: &SseEvent) -> NaviResult<Vec<StreamEvent>> {
        match self {
            Self::OpenAI(decoder) => {
                let payload = if is_done_event(frame) {
                    "[DONE]"
                } else {
                    &frame.data
                };
                decoder.push_chunk(payload)
            }
            Self::Anthropic(decoder) => {
                decoder.push_event(frame.event.as_deref().unwrap_or(""), &frame.data)
            }
            Self::Google(decoder) => decoder.push_chunk(&frame.data),
        }
    }

    /// Closes a stream that ended without an explicit terminal event.
    fn finish(&mut self) -> NaviResult<Vec<StreamEvent>> {
        match self {
            Self::OpenAI(decoder) => decoder.finish(),
            Self::Anthropic(decoder) => decoder.push_event("message_stop", "{}"),
            Self::Google(decoder) => Ok(decoder.finish()),
        }
    }
}

/// Appends decoded text to the raw body buffer and the incremental SSE parser.
fn push_text(parser: &mut SseParser, raw: &mut String, frames: &mut Vec<SseEvent>, text: &str) {
    raw.push_str(text);
    frames.extend(parser.push(text));
}

/// Forwards decoder events to the caller and remembers the terminal message.
fn emit_events(
    events: Vec<StreamEvent>,
    on_event: &mut dyn FnMut(&StreamEvent),
    started: &mut bool,
    final_message: &mut Option<Message>,
) -> NaviResult<()> {
    for event in events {
        if matches!(&event, StreamEvent::Start) {
            *started = true;
        }
        if let StreamEvent::Done { message, .. } = &event {
            *final_message = Some(message.clone());
        }
        on_event(&event);
        if let StreamEvent::Error { error, .. } = &event {
            return Err(NaviError::Provider(error.clone()));
        }
    }
    Ok(())
}

/// Converts a non-streaming JSON response into decoder frames.
fn decode_plain_json(
    kind: ProviderKind,
    raw: &str,
    on_event: &mut dyn FnMut(&StreamEvent),
) -> NaviResult<Message> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| NaviError::Protocol(format!("invalid provider response: {error}")))?;
    let frames = match kind {
        ProviderKind::OpenAiCompat => {
            let mut chunk = Map::new();
            for key in ["id", "model", "usage"] {
                if let Some(field) = value.get(key) {
                    chunk.insert(key.to_owned(), field.clone());
                }
            }
            let choices = value
                .get("choices")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|choice| {
                            let mut converted = Map::new();
                            for key in ["index", "finish_reason", "usage"] {
                                if let Some(field) = choice.get(key) {
                                    converted.insert(key.to_owned(), field.clone());
                                }
                            }
                            if let Some(message) = choice.get("message") {
                                converted.insert("delta".to_owned(), message.clone());
                            }
                            Value::Object(converted)
                        })
                        .collect()
                })
                .unwrap_or_default();
            chunk.insert("choices".to_owned(), Value::Array(choices));
            vec![
                SseEvent {
                    data: Value::Object(chunk).to_string(),
                    ..SseEvent::default()
                },
                SseEvent {
                    data: "[DONE]".to_owned(),
                    ..SseEvent::default()
                },
            ]
        }
        ProviderKind::Anthropic => {
            let mut frames = Vec::new();
            frames.push(SseEvent {
                event: Some("message_start".to_owned()),
                data: json!({ "message": value }).to_string(),
                ..SseEvent::default()
            });
            if let Some(blocks) = value.get("content").and_then(Value::as_array) {
                for (index, block) in blocks.iter().enumerate() {
                    frames.push(SseEvent {
                        event: Some("content_block_start".to_owned()),
                        data: json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": block,
                        })
                        .to_string(),
                        ..SseEvent::default()
                    });
                    frames.push(SseEvent {
                        event: Some("content_block_stop".to_owned()),
                        data: json!({ "type": "content_block_stop", "index": index }).to_string(),
                        ..SseEvent::default()
                    });
                }
            }
            let mut delta = Map::new();
            delta.insert(
                "delta".to_owned(),
                json!({ "stop_reason": value.get("stop_reason").cloned().unwrap_or(Value::Null) }),
            );
            if let Some(usage) = value.get("usage") {
                delta.insert("usage".to_owned(), usage.clone());
            }
            frames.push(SseEvent {
                event: Some("message_delta".to_owned()),
                data: Value::Object(delta).to_string(),
                ..SseEvent::default()
            });
            frames.push(SseEvent {
                event: Some("message_stop".to_owned()),
                data: "{}".to_owned(),
                ..SseEvent::default()
            });
            frames
        }
        ProviderKind::Google => vec![SseEvent {
            data: raw.to_owned(),
            ..SseEvent::default()
        }],
    };

    let mut decoder = Decoder::new(kind);
    let mut started = false;
    let mut final_message = None;
    for frame in frames {
        let events = decoder.push(&frame)?;
        emit_events(events, on_event, &mut started, &mut final_message)?;
    }
    let tail = decoder.finish()?;
    if !started {
        on_event(&StreamEvent::Start);
    }
    emit_events(tail, on_event, &mut started, &mut final_message)?;
    final_message.ok_or_else(|| {
        NaviError::Protocol("provider response did not contain an assistant message".to_owned())
    })
}

/// Percent-encodes a value placed in a single URL path segment.
fn url_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    encoded
}

/// Derives a deterministic-but-varying jitter seed from the clock and attempt.
fn retry_seed(attempt: u32) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_nanos() as u64) ^ u64::from(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::StopReason;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;

    /// One canned HTTP response served by the in-process mock server.
    struct MockResponse {
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        body: String,
        retry_after: Option<&'static str>,
    }

    impl MockResponse {
        /// Creates a successful SSE response.
        fn sse(body: &str) -> Self {
            Self {
                status: 200,
                reason: "OK",
                content_type: "text/event-stream",
                body: body.to_owned(),
                retry_after: None,
            }
        }

        /// Creates a response with an arbitrary status and body.
        fn status(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "application/json",
                body: body.to_owned(),
                retry_after: None,
            }
        }

        /// Attaches a `Retry-After` header to this response.
        fn with_retry_after(mut self, value: &'static str) -> Self {
            self.retry_after = Some(value);
            self
        }
    }

    /// Starts a mock server that serves one canned response per connection.
    fn spawn_mock(responses: Vec<MockResponse>) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("mock server address").port();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let request = read_request(&mut stream);
                let _ = tx.send(request);

                let mut out = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    response.reason,
                    response.content_type,
                    response.body.len()
                )
                .into_bytes();
                if let Some(value) = response.retry_after {
                    out.extend_from_slice(format!("Retry-After: {value}\r\n").as_bytes());
                }
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(response.body.as_bytes());
                let _ = stream.write_all(&out);
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), rx, handle)
    }

    /// Reads one HTTP request, including the body indicated by Content-Length.
    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 2048];
        let header_end = loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return String::from_utf8_lossy(&bytes).into_owned(),
                Ok(count) => {
                    bytes.extend_from_slice(&buf[..count]);
                    if let Some(position) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                }
            }
        };
        let head = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(count) => bytes.extend_from_slice(&buf[..count]),
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Builds a fast client that does not retry unless a test opts in.
    fn test_client(kind: ProviderKind, base_url: &str) -> LlmHttpClient {
        LlmHttpClient::new(LlmEndpointConfig {
            retry_policy: RetryPolicy {
                enabled: false,
                ..RetryPolicy::default()
            },
            ..LlmEndpointConfig::new(kind, base_url, "test-key", "test-model")
        })
    }

    #[test]
    fn openai_streaming_delivers_events_and_final_message() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, requests, server) = spawn_mock(vec![MockResponse::sse(body)]);
        let mut events = Vec::new();

        let message = test_client(ProviderKind::OpenAiCompat, &base_url)
            .complete_streaming(
                Some("be brief"),
                &[Message::user_text("hi", 1)],
                &[],
                &mut |event| events.push(event.clone()),
            )
            .expect("OpenAI stream completes");
        server.join().expect("mock server thread");
        let request = requests.recv().expect("request was recorded");

        assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-key")
        );
        assert!(request.contains("\"stream\":true"));
        assert!(request.contains("be brief"));
        assert_eq!(message.text_content(), "Hello world");
        assert_eq!(message.stop_reason, Some(StopReason::Stop));
        assert_eq!(message.usage.expect("usage").total_tokens, 6);
        assert!(matches!(events.first(), Some(StreamEvent::Start)));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::Done { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TextDelta { delta, .. } if delta == " world"
        )));
    }

    #[test]
    fn anthropic_complete_parses_sse_message() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude\",",
            "\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (base_url, requests, server) = spawn_mock(vec![MockResponse::sse(body)]);

        let message = test_client(ProviderKind::Anthropic, &base_url)
            .complete(None, &[Message::user_text("hello", 1)], &[])
            .expect("Anthropic stream completes");
        server.join().expect("mock server thread");
        let request = requests.recv().expect("request was recorded");

        assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(request.to_ascii_lowercase().contains("x-api-key: test-key"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("anthropic-version: 2023-06-01")
        );
        assert_eq!(message.text_content(), "Hi");
        assert_eq!(message.stop_reason, Some(StopReason::Stop));
        let usage = message.usage.expect("usage");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 10);
    }

    #[test]
    fn google_streaming_parses_gemini_chunks() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Gemini says hi\"}],",
            "\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{",
            "\"promptTokenCount\":3,\"candidatesTokenCount\":4,\"totalTokenCount\":7}}\n\n",
        );
        let (base_url, requests, server) = spawn_mock(vec![MockResponse::sse(body)]);

        let message = test_client(ProviderKind::Google, &base_url)
            .complete(None, &[Message::user_text("hi", 1)], &[])
            .expect("Google stream completes");
        server.join().expect("mock server thread");
        let request = requests.recv().expect("request was recorded");

        assert!(
            request.starts_with("POST /models/test-model:streamGenerateContent?alt=sse HTTP/1.1")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-goog-api-key: test-key")
        );
        assert_eq!(message.text_content(), "Gemini says hi");
        assert_eq!(message.stop_reason, Some(StopReason::Stop));
        let usage = message.usage.expect("usage");
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 4);
    }

    #[test]
    fn http_error_body_is_mapped_to_provider_error_without_retry() {
        let body = r#"{"error":{"message":"Invalid API key","type":"authentication_error","code":"invalid_api_key"}}"#;
        let (base_url, requests, server) =
            spawn_mock(vec![MockResponse::status(401, "Unauthorized", body)]);

        let error = test_client(ProviderKind::OpenAiCompat, &base_url)
            .complete(None, &[Message::user_text("hi", 1)], &[])
            .expect_err("401 must fail");
        server.join().expect("mock server thread");

        match error {
            NaviError::Provider(message) => {
                assert!(message.contains("401"), "message: {message}");
                assert!(message.contains("Invalid API key"), "message: {message}");
                assert!(message.contains("invalid_api_key"), "message: {message}");
            }
            other => panic!("expected provider error, got {other:?}"),
        }
        assert!(requests.recv().is_ok());
        assert!(
            requests.try_recv().is_err(),
            "non-retryable status must not be retried"
        );
    }

    #[test]
    fn transient_http_failure_is_retried() {
        let busy = MockResponse::status(503, "Service Unavailable", "busy").with_retry_after("0");
        let ok = MockResponse::sse(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Recovered\"}}]}\n\n",
            "data: [DONE]\n\n",
        ));
        let (base_url, requests, server) = spawn_mock(vec![busy, ok]);
        let client = LlmHttpClient::new(LlmEndpointConfig {
            retry_policy: RetryPolicy {
                enabled: true,
                max_retries: 1,
                base_delay_ms: 10,
                max_delay_ms: Some(0),
            },
            ..LlmEndpointConfig::new(
                ProviderKind::OpenAiCompat,
                &base_url,
                "test-key",
                "test-model",
            )
        });

        let message = client
            .complete(None, &[Message::user_text("hi", 1)], &[])
            .expect("second attempt succeeds");
        server.join().expect("mock server thread");

        assert_eq!(message.text_content(), "Recovered");
        assert!(requests.recv().is_ok());
        assert!(requests.recv().is_ok());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn invalid_endpoint_config_is_rejected_before_network_use() {
        let client = test_client(ProviderKind::OpenAiCompat, "   ");
        let error = client
            .complete(None, &[], &[])
            .expect_err("empty base URL is invalid");
        assert!(matches!(error, NaviError::InvalidInput(_)));

        let client = LlmHttpClient::new(LlmEndpointConfig {
            model: String::new(),
            ..LlmEndpointConfig::new(ProviderKind::Anthropic, "http://127.0.0.1:1", "k", "m")
        });
        let error = client
            .complete(None, &[], &[])
            .expect_err("empty model is invalid");
        assert!(matches!(error, NaviError::InvalidInput(_)));
    }

    #[test]
    fn plain_json_response_is_accepted() {
        let body = json!({
            "id": "chat_1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Plain answer" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5 },
        })
        .to_string();
        let (base_url, requests, server) = spawn_mock(vec![MockResponse::sse(&body)]);

        let message = test_client(ProviderKind::OpenAiCompat, &base_url)
            .complete(None, &[Message::user_text("hi", 1)], &[])
            .expect("plain JSON response is decoded");
        server.join().expect("mock server thread");
        let request = requests.recv().expect("request was recorded");

        assert!(request.contains("\"stream\":true"));
        assert_eq!(message.text_content(), "Plain answer");
        assert_eq!(message.stop_reason, Some(StopReason::Stop));
        assert_eq!(message.usage.expect("usage").total_tokens, 5);
        assert_eq!(message.metadata["response_id"], json!("chat_1"));
    }

    #[test]
    fn stream_error_reason_is_reported_as_provider_error() {
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},",
            "\"finish_reason\":\"content_filter\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, _requests, server) = spawn_mock(vec![MockResponse::sse(body)]);
        let mut events = Vec::new();

        let error = test_client(ProviderKind::OpenAiCompat, &base_url)
            .complete_streaming(None, &[], &[], &mut |event| events.push(event.clone()))
            .expect_err("error finish reason fails the call");
        server.join().expect("mock server thread");

        match error {
            NaviError::Provider(message) => {
                assert!(message.contains("content_filter"), "message: {message}");
            }
            other => panic!("expected provider error, got {other:?}"),
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::Error { .. }))
        );
    }
}
