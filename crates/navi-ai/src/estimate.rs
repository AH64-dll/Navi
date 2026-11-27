//! Token estimation and USD cost accounting, ported from
//! `packages/ai/src/utils/estimate.ts` and `packages/ai/src/models.ts`.

use navi_types::{ContentBlock, Message, Role, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Average number of characters the heuristic assumes per token.
const CHARS_PER_TOKEN: f64 = 3.5;

/// Character count assigned to an image block when estimating tokens.
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

/// USD price per one million tokens for each usage category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Price per million non-cached input tokens.
    pub input_per_million: f64,
    /// Price per million output tokens.
    pub output_per_million: f64,
    /// Price per million cache-read tokens.
    pub cache_read_per_million: f64,
    /// Price per million cache-write tokens.
    pub cache_write_per_million: f64,
}

/// Estimate the number of tokens in a plain text string.
pub fn estimate_text_tokens(text: &str) -> u64 {
    ceil_tokens(text.chars().count() as u64)
}

/// Estimate the number of tokens contributed by a single message.
pub fn estimate_message_tokens(msg: &Message) -> u64 {
    match msg.role {
        Role::System => {
            let mut chars = system_message_text(msg).chars().count() as u64;
            if let Some(tools) = msg.metadata.get("toolsAdded") {
                chars = chars.saturating_add(serialized_len(tools));
            }
            if let Some(tools) = msg.metadata.get("toolsRemoved") {
                chars = chars.saturating_add(serialized_len(tools));
            }
            ceil_tokens(chars)
        }
        Role::User | Role::ToolResult => ceil_tokens(estimate_text_and_image_chars(&msg.content)),
        Role::Assistant => {
            let mut chars = 0u64;
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        chars = chars.saturating_add(text.chars().count() as u64);
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        chars = chars.saturating_add(thinking.chars().count() as u64);
                    }
                    ContentBlock::ToolCall(call) => {
                        chars = chars.saturating_add(call.name.chars().count() as u64);
                        chars = chars.saturating_add(serialized_len(&call.arguments));
                    }
                    ContentBlock::ToolResult(result) => {
                        chars = chars.saturating_add(result.tool_name.chars().count() as u64);
                        chars = chars.saturating_add(serialized_len(&result.content));
                    }
                    ContentBlock::Image { .. } => {
                        chars = chars.saturating_add(ESTIMATED_IMAGE_CHARS);
                    }
                }
            }
            ceil_tokens(chars)
        }
    }
}

/// Estimate the total number of tokens across a sequence of messages.
pub fn estimate_messages_tokens(msgs: &[Message]) -> u64 {
    let mut total = 0u64;
    for msg in msgs {
        total = total.saturating_add(estimate_message_tokens(msg));
    }
    total
}

/// Compute the USD cost of a usage record from per-million-token prices.
///
/// Prior values in `usage.cost_usd` and `usage.total_tokens` are ignored; the
/// cost is derived solely from the four token categories and the supplied rates.
pub fn calculate_cost_usd(usage: &Usage, pricing: &ModelPricing) -> f64 {
    let input = rate_cost(usage.input_tokens, pricing.input_per_million);
    let output = rate_cost(usage.output_tokens, pricing.output_per_million);
    let cache_read = rate_cost(usage.cache_read_tokens, pricing.cache_read_per_million);
    let cache_write = rate_cost(usage.cache_write_tokens, pricing.cache_write_per_million);
    input + output + cache_read + cache_write
}

fn rate_cost(tokens: u64, per_million: f64) -> f64 {
    (tokens as f64 / 1_000_000.0) * per_million
}

fn ceil_tokens(chars: u64) -> u64 {
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

fn estimate_text_and_image_chars(content: &[ContentBlock]) -> u64 {
    let mut chars = 0u64;
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                chars = chars.saturating_add(text.chars().count() as u64);
            }
            // The reference treats every non-text block as an image.
            _ => chars = chars.saturating_add(ESTIMATED_IMAGE_CHARS),
        }
    }
    chars
}

fn content_text(content: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in content {
        if let ContentBlock::Text { text } = block {
            parts.push(text.as_str());
        }
    }
    parts.join("\n")
}

fn system_message_text(msg: &Message) -> String {
    let mut parts = vec![content_text(&msg.content)];
    if let Some(Value::Object(sections)) = msg.metadata.get("sections") {
        for value in sections.values() {
            if value.is_null() {
                continue;
            }
            parts.push(match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
        }
    }
    parts.retain(|part| !part.is_empty());
    parts.join("\n\n")
}

fn serialized_len<T: Serialize>(value: &T) -> u64 {
    match serde_json::to_string(value) {
        Ok(json) => json.chars().count() as u64,
        Err(_) => "[unserializable]".len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::{StopReason, ToolCall, ToolResult};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn estimate_text_tokens_basics() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("a"), 1);
        assert_eq!(estimate_text_tokens("abc"), 1);
        assert_eq!(estimate_text_tokens("abcd"), 2);
        assert_eq!(estimate_text_tokens("abcdefg"), 2);
        assert_eq!(estimate_text_tokens("abcdefgh"), 3);
    }

    #[test]
    fn estimate_text_tokens_counts_unicode_scalars() {
        assert_eq!(estimate_text_tokens("héllo"), 2);
        assert_eq!(estimate_text_tokens("😀😀"), 1);
    }

    #[test]
    fn estimate_message_tokens_user_and_empty_content() {
        assert_eq!(
            estimate_message_tokens(&Message::user_text("hello world", 0)),
            4
        );
        assert_eq!(estimate_message_tokens(&Message::user_text("", 0)), 0);
    }

    #[test]
    fn estimate_message_tokens_assistant_blocks() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "abc".into() },
                ContentBlock::Thinking {
                    thinking: "defgh".into(),
                    signature: None,
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "1".into(),
                    name: "go".into(),
                    arguments: json!({}),
                }),
            ],
            timestamp_ms: 0,
            stop_reason: Some(StopReason::Stop),
            usage: None,
            metadata: BTreeMap::new(),
        };
        // 3 (text) + 5 (thinking) + 2 (name) + 2 ("{}") = 12 chars -> ceil(12/3.5) = 4.
        assert_eq!(estimate_message_tokens(&msg), 4);
    }

    #[test]
    fn estimate_message_tokens_assistant_tool_result_block() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_call_id: "call_1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::Text { text: "abc".into() }],
                is_error: false,
                details: None,
            })],
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        };
        // 4 ("read") + 30 (`[{"type":"text","text":"abc"}]`) = 34 chars -> ceil(34/3.5) = 10.
        assert_eq!(estimate_message_tokens(&msg), 10);
    }

    #[test]
    fn estimate_message_tokens_image_content() {
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    data: "x".into(),
                    mime_type: "image/png".into(),
                },
                ContentBlock::Text { text: "abc".into() },
            ],
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        };
        // 4800 (image) + 3 (text) = 4803 chars -> ceil(4803/3.5) = 1373.
        assert_eq!(estimate_message_tokens(&msg), 1373);
    }

    #[test]
    fn estimate_message_tokens_system_includes_tools() {
        let mut metadata = BTreeMap::new();
        metadata.insert("toolsAdded".to_string(), json!([{"name":"x"}]));
        let msg = Message {
            role: Role::System,
            content: vec![ContentBlock::Text { text: "abc".into() }],
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata,
        };
        // 3 (text) + 14 (`[{"name":"x"}]`) = 17 chars -> ceil(17/3.5) = 5.
        assert_eq!(estimate_message_tokens(&msg), 5);
    }

    #[test]
    fn estimate_messages_tokens_sums_messages() {
        let msgs = vec![
            Message::user_text("hello", 0),
            Message::assistant_text("world!!", 1),
        ];
        assert_eq!(estimate_messages_tokens(&msgs), 4);
        assert_eq!(estimate_messages_tokens(&[]), 0);
    }

    #[test]
    fn calculate_cost_usd_all_categories() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 250_000,
            total_tokens: 3_750_000,
            cost_usd: 999.0,
        };
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: 0.3,
            cache_write_per_million: 3.75,
        };
        let cost = calculate_cost_usd(&usage, &pricing);
        assert!((cost - 12.0375).abs() < 1e-12, "cost was {cost}");
    }

    #[test]
    fn calculate_cost_usd_zero_usage_and_zero_pricing() {
        let priced = ModelPricing {
            input_per_million: 5.0,
            output_per_million: 9.0,
            ..ModelPricing::default()
        };
        assert_eq!(calculate_cost_usd(&Usage::default(), &priced), 0.0);
        assert_eq!(
            calculate_cost_usd(
                &Usage {
                    input_tokens: 1000,
                    ..Usage::default()
                },
                &ModelPricing::default()
            ),
            0.0
        );
    }

    #[test]
    fn calculate_cost_usd_ignores_total_tokens_and_reported_cost() {
        let usage = Usage {
            total_tokens: u64::MAX,
            cost_usd: 42.0,
            ..Usage::default()
        };
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 1.0,
            cache_read_per_million: 1.0,
            cache_write_per_million: 1.0,
        };
        assert_eq!(calculate_cost_usd(&usage, &pricing), 0.0);
    }
}
