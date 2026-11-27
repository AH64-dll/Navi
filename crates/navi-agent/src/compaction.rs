//! Context-window compaction for long agent sessions.
//!
//! Planning is pure: it estimates the size of a transcript, decides whether the
//! window is over budget, and picks the index of the first message to keep.
//! A cut is always placed at a user or assistant message, and is repaired so a
//! tool call and its result are never separated by the boundary. Applying a
//! plan replaces the dropped prefix with a summary supplied by the caller; the
//! caller owns summarization I/O and the system prompt replay.

use std::collections::{BTreeMap, BTreeSet};

use navi_types::{ContentBlock, Message, Role, StopReason, Usage};

/// Characters assumed for an image block when estimating token counts.
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

/// Token thresholds that drive context compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Size of the model context window, in tokens.
    pub max_context_tokens: u64,
    /// Headroom left for the summary and the next model response.
    pub reserve_tokens: u64,
    /// Approximate size of the most recent tail kept verbatim, in tokens.
    pub keep_recent_tokens: u64,
}

/// Outcome of planning a compaction.
///
/// `dropped_messages` and `kept_messages` partition the input transcript at
/// `cut_index`: the dropped prefix is `[0, cut_index)` and the kept tail is
/// `[cut_index, len)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Whether the transcript exceeds the usable context window.
    pub should_compact: bool,
    /// Index of the first kept message, or zero when nothing is cut.
    pub cut_index: usize,
    /// Number of messages replaced by the summary.
    pub dropped_messages: usize,
    /// Number of messages kept verbatim.
    pub kept_messages: usize,
    /// Estimated size of the whole transcript before compaction.
    pub estimated_tokens_before: u64,
}

/// Estimate the tokens occupied by one message using the chars/4 heuristic.
///
/// The estimate is deliberately conservative: images count as a fixed number
/// of characters, thinking and text count their length, and tool calls count
/// their name plus their serialized arguments.
pub fn estimate_tokens(message: &Message) -> u64 {
    block_chars(&message.content).div_ceil(4)
}

/// Estimate the transcript size, preferring the last valid assistant usage.
///
/// Usage recorded by the model is more accurate than a character estimate for
/// the history it covers, so only messages after that assistant are estimated.
/// Aborted, errored and all-zero usage records are ignored.
pub fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let last_usage = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| assistant_tokens(message).map(|tokens| (index, tokens)));

    match last_usage {
        Some((index, usage_tokens)) => {
            let trailing = messages[index + 1..]
                .iter()
                .map(estimate_tokens)
                .fold(0u64, u64::saturating_add);
            usage_tokens.saturating_add(trailing)
        }
        None => messages
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add),
    }
}

/// Report whether a transcript of `tokens` exceeds the usable window.
pub fn should_compact(tokens: u64, cfg: &CompactionConfig) -> bool {
    let usable = cfg.max_context_tokens.saturating_sub(cfg.reserve_tokens);
    tokens > usable
}

/// Build a compaction plan for `messages` under `cfg`.
///
/// When compaction is needed, the cut is chosen so roughly
/// `keep_recent_tokens` of the newest transcript remains, then moved back if
/// necessary to keep every tool call together with its result.
pub fn plan_compaction(messages: &[Message], cfg: &CompactionConfig) -> CompactionPlan {
    let estimated_tokens_before = estimate_context_tokens(messages);
    let should_compact = should_compact(estimated_tokens_before, cfg);
    let cut_points = valid_cut_points(messages);

    let cut_index = if should_compact && !cut_points.is_empty() {
        let provisional = provisional_cut(messages, &cut_points, cfg.keep_recent_tokens);
        repair_cut(messages, provisional, &cut_points)
    } else {
        0
    };

    CompactionPlan {
        should_compact,
        cut_index,
        dropped_messages: cut_index,
        kept_messages: messages.len().saturating_sub(cut_index),
        estimated_tokens_before,
    }
}

/// Replace the dropped prefix with `summary` and return the compacted history.
///
/// An inactive or stale plan (nothing dropped, or a cut past the end of the
/// slice) leaves the transcript untouched. With an active plan the summary is
/// emitted as a user message carrying the timestamp of the last dropped
/// message, followed by the kept tail. Tool results in the tail are moved to
/// sit directly after the assistant call they answer, so the cut never leaves
/// a call separated from its result.
pub fn apply_compaction(
    messages: &[Message],
    plan: &CompactionPlan,
    summary: &str,
) -> Vec<Message> {
    if !plan.should_compact || plan.cut_index == 0 || plan.cut_index >= messages.len() {
        return messages.to_vec();
    }

    let cut = plan.cut_index;
    let timestamp_ms = messages[cut - 1].timestamp_ms;
    let kept = order_tool_results(&messages[cut..]);
    let mut compacted = Vec::with_capacity(kept.len() + 1);
    compacted.push(Message::user_text(summary, timestamp_ms));
    compacted.extend(kept);
    compacted
}

/// Move each tool result to just after the assistant call it answers.
///
/// A cut can land between a call and its result when an unrelated message
/// interleaves them. Model APIs expect the result to follow the call directly,
/// so those results are hoisted. A result whose call is not kept, or that
/// already precedes its call, stays at its original position. Every input
/// message is emitted exactly once.
fn order_tool_results(kept: &[Message]) -> Vec<Message> {
    let mut results_by_call: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, message) in kept.iter().enumerate() {
        for block in &message.content {
            if let ContentBlock::ToolResult(result) = block {
                results_by_call
                    .entry(result.tool_call_id.as_str())
                    .or_default()
                    .push(index);
            }
        }
    }

    let mut hoisted = vec![false; kept.len()];
    let mut ordered = Vec::with_capacity(kept.len());
    for (index, message) in kept.iter().enumerate() {
        if hoisted[index] {
            continue;
        }
        ordered.push(message.clone());
        for block in &message.content {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            let Some(result_indices) = results_by_call.get(call.id.as_str()) else {
                continue;
            };
            for &result_index in result_indices {
                if result_index > index && !hoisted[result_index] {
                    ordered.push(kept[result_index].clone());
                    hoisted[result_index] = true;
                }
            }
        }
    }
    ordered
}

/// Count the estimated characters contributed by a list of content blocks.
fn block_chars(blocks: &[ContentBlock]) -> u64 {
    let mut chars = 0u64;
    for block in blocks {
        match block {
            ContentBlock::Text { text } => chars = chars.saturating_add(text.len() as u64),
            ContentBlock::Thinking { thinking, .. } => {
                chars = chars.saturating_add(thinking.len() as u64);
            }
            ContentBlock::Image { .. } => chars = chars.saturating_add(ESTIMATED_IMAGE_CHARS),
            ContentBlock::ToolCall(call) => {
                chars = chars.saturating_add(call.name.len() as u64);
                let arguments_len =
                    serde_json::to_string(&call.arguments).map_or(0, |json| json.len() as u64);
                chars = chars.saturating_add(arguments_len);
            }
            ContentBlock::ToolResult(result) => {
                chars = chars.saturating_add(block_chars(&result.content));
            }
        }
    }
    chars
}

/// Return usable usage tokens for an assistant message, if it has any.
fn assistant_tokens(message: &Message) -> Option<u64> {
    if message.role != Role::Assistant {
        return None;
    }
    if matches!(
        message.stop_reason,
        Some(StopReason::Aborted | StopReason::Error)
    ) {
        return None;
    }
    let tokens = calculate_context_tokens(message.usage.as_ref()?);
    (tokens > 0).then_some(tokens)
}

/// Prefer the recorded total, falling back to the sum of usage components.
fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        return usage.total_tokens;
    }
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
}

/// Indices at which a cut may be placed.
///
/// Tool results and system messages are never cut points: results belong to
/// the assistant call that precedes them, and the system prompt is replayed by
/// the caller rather than summarized.
fn valid_cut_points(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message.role, Role::User | Role::Assistant))
        .map(|(index, _)| index)
        .collect()
}

/// Find the first cut point that leaves roughly `keep_recent_tokens` behind.
fn provisional_cut(messages: &[Message], cut_points: &[usize], keep_recent_tokens: u64) -> usize {
    let fallback = cut_points[cut_points.len() - 1];
    let mut accumulated = 0u64;

    for index in (0..messages.len()).rev() {
        let tokens = estimate_tokens(&messages[index]);
        if tokens == 0 {
            continue;
        }
        accumulated = accumulated.saturating_add(tokens);
        if accumulated >= keep_recent_tokens {
            return cut_points
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
                .unwrap_or(fallback);
        }
    }

    cut_points[0]
}

/// Move a cut back until no result in the kept tail belongs to a dropped call.
fn repair_cut(messages: &[Message], mut cut: usize, cut_points: &[usize]) -> usize {
    while let Some(call_id) = split_call_id(messages, cut) {
        let owner = messages[..cut]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                let issued = message.content.iter().any(
                    |block| matches!(block, ContentBlock::ToolCall(call) if call.id == call_id),
                );
                issued.then_some(index)
            });
        let Some(owner) = owner else {
            break;
        };
        cut = cut_points
            .iter()
            .copied()
            .rfind(|candidate| *candidate <= owner)
            .unwrap_or(0);
    }
    cut
}

/// Return a call id whose result would land on the other side of `cut`.
fn split_call_id(messages: &[Message], cut: usize) -> Option<String> {
    let kept_results: BTreeSet<&str> = messages[cut..]
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();

    messages[..cut]
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) if kept_results.contains(call.id.as_str()) => {
                Some(call.id.clone())
            }
            _ => None,
        })
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::{ToolCall, ToolResult};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn user(text: impl Into<String>, timestamp_ms: u64) -> Message {
        Message::user_text(text, timestamp_ms)
    }

    fn assistant(text: impl Into<String>, timestamp_ms: u64) -> Message {
        Message::assistant_text(text, timestamp_ms)
    }

    fn long_user(timestamp_ms: u64) -> Message {
        user("x".repeat(400), timestamp_ms)
    }

    fn assistant_call(id: &str, timestamp_ms: u64) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "x"}),
            })],
            timestamp_ms,
            stop_reason: Some(StopReason::ToolUse),
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    fn tool_result(id: &str, timestamp_ms: u64) -> Message {
        Message {
            role: Role::ToolResult,
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_call_id: id.to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                is_error: false,
                details: None,
            })],
            timestamp_ms,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    fn cfg(max: u64, reserve: u64, keep: u64) -> CompactionConfig {
        CompactionConfig {
            max_context_tokens: max,
            reserve_tokens: reserve,
            keep_recent_tokens: keep,
        }
    }

    #[test]
    fn empty_transcript_is_not_compacted() {
        let plan = plan_compaction(&[], &cfg(100, 10, 40));
        assert_eq!(
            plan,
            CompactionPlan {
                should_compact: false,
                cut_index: 0,
                dropped_messages: 0,
                kept_messages: 0,
                estimated_tokens_before: 0,
            }
        );
        assert!(apply_compaction(&[], &plan, "summary").is_empty());
    }

    #[test]
    fn small_transcript_stays_below_the_threshold() {
        let messages = vec![user("short", 1), assistant("also short", 2)];
        let plan = plan_compaction(&messages, &cfg(100, 10, 40));
        assert!(!plan.should_compact);
        assert_eq!(plan.cut_index, 0);
        assert_eq!(plan.dropped_messages, 0);
        assert_eq!(plan.kept_messages, 2);
        assert_eq!(apply_compaction(&messages, &plan, "summary"), messages);
    }

    #[test]
    fn large_transcript_drops_a_prefix_and_keeps_the_recent_tail() {
        let messages: Vec<Message> = (0..8)
            .map(|i| {
                if i % 2 == 0 {
                    long_user(i)
                } else {
                    assistant("y".repeat(400), i)
                }
            })
            .collect();
        let plan = plan_compaction(&messages, &cfg(500, 100, 250));

        assert!(plan.should_compact);
        assert_eq!(plan.estimated_tokens_before, 800);
        assert_eq!(plan.cut_index, 5);
        assert_eq!(plan.dropped_messages, 5);
        assert_eq!(plan.kept_messages, 3);
        assert_eq!(plan.dropped_messages + plan.kept_messages, messages.len());

        let compacted = apply_compaction(&messages, &plan, "checkpoint");
        assert_eq!(compacted.len(), 4);
        assert_eq!(compacted[0].role, Role::User);
        assert_eq!(compacted[0].text_content(), "checkpoint");
        assert_eq!(&compacted[1..], &messages[plan.cut_index..]);
    }

    #[test]
    fn trailing_tool_results_keep_their_assistant_call() {
        let messages = vec![
            long_user(0),
            assistant_call("a", 1),
            tool_result("a", 2),
            long_user(3),
            assistant_call("b", 4),
            tool_result("b", 5),
        ];
        let plan = plan_compaction(&messages, &cfg(150, 50, 1));

        assert!(plan.should_compact);
        assert_eq!(plan.cut_index, 4);
        assert_ne!(messages[plan.cut_index].role, Role::ToolResult);

        let compacted = apply_compaction(&messages, &plan, "checkpoint");
        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[1].role, Role::Assistant);
        assert_eq!(compacted[2].role, Role::ToolResult);
        assert_eq!(compacted[1].tool_calls()[0].id, "b");
        match &compacted[2].content[0] {
            ContentBlock::ToolResult(result) => assert_eq!(result.tool_call_id, "b"),
            other => panic!("expected a tool result block, got {other:?}"),
        }
    }

    #[test]
    fn cut_is_repaired_when_a_user_message_interleaves_a_tool_pair() {
        let messages = vec![
            user("u0", 0),
            assistant_call("x", 1),
            long_user(2),
            tool_result("x", 3),
            assistant("final", 4),
        ];
        let plan = plan_compaction(&messages, &cfg(200, 100, 102));

        assert!(plan.should_compact);
        assert_eq!(plan.cut_index, 1);
        assert_eq!(plan.dropped_messages, 1);
        assert_eq!(plan.kept_messages, 4);

        let compacted = apply_compaction(&messages, &plan, "checkpoint");
        assert_eq!(compacted[1].tool_calls()[0].id, "x");
        match &compacted[2].content[0] {
            ContentBlock::ToolResult(result) => assert_eq!(result.tool_call_id, "x"),
            other => panic!("expected a tool result block, got {other:?}"),
        }
    }

    #[test]
    fn tool_results_are_hoisted_without_dropping_interleaving_messages() {
        let kept = vec![
            assistant_call("a", 0),
            user("between a", 1),
            tool_result("a", 2),
            assistant_call("b", 3),
            user("between b", 4),
            tool_result("b", 5),
            tool_result("orphan", 6),
        ];

        let ordered = order_tool_results(&kept);
        assert_eq!(ordered.len(), kept.len());
        assert_eq!(ordered[0].tool_calls()[0].id, "a");
        match &ordered[1].content[0] {
            ContentBlock::ToolResult(result) => assert_eq!(result.tool_call_id, "a"),
            other => panic!("expected a tool result block, got {other:?}"),
        }
        assert_eq!(ordered[2].text_content(), "between a");
        assert_eq!(ordered[3].tool_calls()[0].id, "b");
        match &ordered[4].content[0] {
            ContentBlock::ToolResult(result) => assert_eq!(result.tool_call_id, "b"),
            other => panic!("expected a tool result block, got {other:?}"),
        }
        assert_eq!(ordered[5].text_content(), "between b");
        match &ordered[6].content[0] {
            ContentBlock::ToolResult(result) => assert_eq!(result.tool_call_id, "orphan"),
            other => panic!("expected a tool result block, got {other:?}"),
        }
    }

    #[test]
    fn transcripts_without_cut_points_are_kept_intact() {
        let messages = vec![
            Message {
                role: Role::ToolResult,
                content: vec![ContentBlock::Text {
                    text: "z".repeat(400),
                }],
                timestamp_ms: 0,
                stop_reason: None,
                usage: None,
                metadata: BTreeMap::new(),
            },
            tool_result("orphan", 1),
        ];
        let plan = plan_compaction(&messages, &cfg(100, 0, 10));

        assert!(plan.should_compact);
        assert_eq!(plan.cut_index, 0);
        assert_eq!(plan.kept_messages, 2);
        assert_eq!(apply_compaction(&messages, &plan, "checkpoint"), messages);
    }

    #[test]
    fn usage_is_preferred_over_character_estimates() {
        let mut with_usage = assistant("q".repeat(40), 1);
        with_usage.stop_reason = Some(StopReason::Stop);
        with_usage.usage = Some(Usage {
            total_tokens: 500,
            ..Usage::default()
        });
        let mut stale_usage = assistant("w".repeat(40), 3);
        stale_usage.stop_reason = Some(StopReason::Error);
        stale_usage.usage = Some(Usage {
            total_tokens: 999,
            ..Usage::default()
        });
        let messages = vec![
            user("a", 0),
            with_usage,
            user("b", 2),
            stale_usage,
            user("c", 4),
        ];

        assert_eq!(estimate_context_tokens(&messages), 512);
    }

    #[test]
    fn usage_total_falls_back_to_its_components() {
        let mut message = assistant("ok", 1);
        message.usage = Some(Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_write_tokens: 5,
            total_tokens: 0,
            cost_usd: 0.0,
        });
        assert_eq!(estimate_context_tokens(&[message]), 155);
    }

    #[test]
    fn block_estimation_counts_thinking_images_and_tools() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "a".repeat(40),
                },
                ContentBlock::Thinking {
                    thinking: "b".repeat(20),
                    signature: None,
                },
                ContentBlock::Image {
                    data: String::new(),
                    mime_type: "image/png".to_string(),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({}),
                }),
                ContentBlock::ToolResult(ToolResult {
                    tool_call_id: "1".to_string(),
                    tool_name: "read".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "ok".to_string(),
                    }],
                    is_error: false,
                    details: None,
                }),
            ],
            timestamp_ms: 0,
            stop_reason: Some(StopReason::ToolUse),
            usage: None,
            metadata: BTreeMap::new(),
        };

        // 40 text + 20 thinking + 4800 image + 6 call + 2 result = 4868 chars.
        assert_eq!(estimate_tokens(&message), 1217);
    }

    #[test]
    fn reserve_larger_than_window_saturates_instead_of_underflowing() {
        let cfg = cfg(10, 50, 20);
        assert!(!should_compact(0, &cfg));
        assert!(should_compact(1, &cfg));
        assert!(!plan_compaction(&[], &cfg).should_compact);
    }

    #[test]
    fn stale_plans_leave_the_transcript_untouched() {
        let messages = vec![user("one", 1), assistant("two", 2)];
        let inactive = CompactionPlan {
            should_compact: false,
            cut_index: 1,
            dropped_messages: 1,
            kept_messages: 1,
            estimated_tokens_before: 2,
        };
        let stale = CompactionPlan {
            should_compact: true,
            cut_index: messages.len(),
            dropped_messages: messages.len(),
            kept_messages: 0,
            estimated_tokens_before: 2,
        };
        assert_eq!(
            apply_compaction(&messages, &inactive, "checkpoint"),
            messages
        );
        assert_eq!(apply_compaction(&messages, &stale, "checkpoint"), messages);
    }
}
