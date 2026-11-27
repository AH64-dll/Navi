//! Frozen Wave 0 shared domain types for the Navi Rust workspace.
//!
//! All teams (`navi-ai`, `navi-agent`, `navi-voice`, `navi-connectors`,
//! `navi-browser`, `navi-sandbox`, `navi-hands`, `navid`) share these types
//! to prevent cross-team struct drift.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// Unique identifier for a conversation session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    /// Creates a new [`SessionId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifies an LLM provider and model pair (for example `anthropic/claude-sonnet-4-5`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl ModelRef {
    /// Creates a [`ModelRef`] from provider and model identifiers.
    pub fn new(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model_id: model_id.into(),
            base_url: None,
        }
    }

    /// Parses `"provider/model"` into a [`ModelRef`].
    pub fn parse(spec: &str) -> Result<Self, NaviError> {
        let Some((provider, model_id)) = spec.split_once('/') else {
            return Err(NaviError::InvalidInput(format!(
                "invalid model ref '{spec}': expected 'provider/model'"
            )));
        };
        if provider.trim().is_empty() || model_id.trim().is_empty() {
            return Err(NaviError::InvalidInput(format!(
                "invalid model ref '{spec}': empty provider or model"
            )));
        }
        Ok(Self::new(provider.trim(), model_id.trim()))
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model_id)
    }
}

/// Conversation role for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    ToolResult,
}

/// Structured content block inside a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Image {
        data: String,
        mime_type: String,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

/// A model-requested tool invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The result returned after executing a [`ToolCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// Schema description for a callable tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Token usage and cost accounting for a model response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
}

impl Usage {
    /// Accumulates another [`Usage`] record into this one.
    pub fn add_assign(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cost_usd += other.cost_usd;
    }
}

/// Reason a model turn terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

/// A single turn message exchanged between user, assistant, and tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

impl Message {
    /// Constructs a plain text user message.
    pub fn user_text(text: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp_ms,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Constructs a plain text assistant message.
    pub fn assistant_text(text: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp_ms,
            stop_reason: Some(StopReason::Stop),
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Concatenates all text blocks in this message.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    /// Extracts all tool calls requested by this message.
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect()
    }
}

/// Streaming events emitted by `navi-ai` and consumed by `navi-agent` / `navid`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start,
    TextDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallStart {
        content_index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        reason: StopReason,
        message: Message,
    },
    Error {
        reason: StopReason,
        error: String,
    },
}

/// Unified workspace error type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum NaviError {
    InvalidInput(String),
    NotFound(String),
    PermissionDenied(String),
    Timeout(String),
    Transport(String),
    Provider(String),
    Protocol(String),
    Io(String),
    Internal(String),
}

impl fmt::Display for NaviError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "invalid input: {m}"),
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::PermissionDenied(m) => write!(f, "permission denied: {m}"),
            Self::Timeout(m) => write!(f, "timeout: {m}"),
            Self::Transport(m) => write!(f, "transport error: {m}"),
            Self::Provider(m) => write!(f, "provider error: {m}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
            Self::Io(m) => write!(f, "io error: {m}"),
            Self::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for NaviError {}

impl From<std::io::Error> for NaviError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<serde_json::Error> for NaviError {
    fn from(err: serde_json::Error) -> Self {
        Self::Protocol(err.to_string())
    }
}

/// Convenience result alias for workspace crates.
pub type NaviResult<T> = Result<T, NaviError>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_ref_parse_roundtrip() {
        let m = ModelRef::parse("opencode-go/deepseek-v4.1-flash").expect("valid ref");
        assert_eq!(m.provider, "opencode-go");
        assert_eq!(m.model_id, "deepseek-v4.1-flash");
        assert_eq!(m.to_string(), "opencode-go/deepseek-v4.1-flash");
        assert!(ModelRef::parse("missing-slash").is_err());
        assert!(ModelRef::parse("/empty-provider").is_err());
    }

    #[test]
    fn message_text_and_tool_calls() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Checking file. ".into(),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "Cargo.toml"}),
                }),
            ],
            timestamp_ms: 1_700_000_000_000,
            stop_reason: Some(StopReason::ToolUse),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Usage::default()
            }),
            metadata: BTreeMap::new(),
        };
        assert_eq!(msg.text_content(), "Checking file. ");
        assert_eq!(msg.tool_calls().len(), 1);
        let serialized = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(parsed, msg);
    }

    #[test]
    fn usage_accumulation() {
        let mut a = Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cost_usd: 0.001,
            ..Usage::default()
        };
        let b = Usage {
            input_tokens: 50,
            output_tokens: 30,
            total_tokens: 80,
            cost_usd: 0.002,
            ..Usage::default()
        };
        a.add_assign(&b);
        assert_eq!(a.input_tokens, 150);
        assert_eq!(a.output_tokens, 50);
        assert_eq!(a.total_tokens, 200);
        assert!((a.cost_usd - 0.003).abs() < 1e-12);
    }
}
