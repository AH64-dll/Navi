//! Cross-provider transcript normalization for the shared [`navi_types::Message`] model.
//!
//! Providers disagree about how strictly tool calls and tool results must be paired and
//! about how consecutive same-role text messages are represented. The helpers here operate
//! on the provider-independent representation: a tool call is a [`ContentBlock::ToolCall`]
//! inside an assistant message, and a result is a [`ContentBlock::ToolResult`] carrying a
//! `tool_call_id`. Matching is by that identifier, not by position, so interleaved system
//! messages and reordered results remain safe.

use navi_types::{ContentBlock, Message, Role, ToolCall, ToolResult};
use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall clock in milliseconds, used only for synthetic results.
///
/// A clock before the Unix epoch is treated as zero rather than failing the normalization.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Collects the identifiers of every tool result present in the transcript.
fn result_ids(messages: &[Message]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult(result) = block {
                ids.insert(result.tool_call_id.clone());
            }
        }
    }
    ids
}

/// Builds the error result a provider needs when the model called a tool that was never
/// answered. Keeping the identifier inside a [`ContentBlock::ToolResult`] lets a later
/// normalization pass recognize the synthetic result as a real match.
fn synthetic_result(call: &ToolCall) -> Message {
    Message {
        role: Role::ToolResult,
        content: vec![ContentBlock::ToolResult(ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![ContentBlock::Text {
                text: "No result provided".to_owned(),
            }],
            is_error: true,
            details: None,
        })],
        timestamp_ms: now_ms(),
        stop_reason: None,
        usage: None,
        metadata: BTreeMap::new(),
    }
}

/// Drops tool results whose `tool_call_id` was never declared by an assistant tool call.
///
/// A call declared in the same message as its result counts as declared. A result that
/// appears before its call is an orphan: it cannot be replayed by providers that require
/// the call first. Results without an explicit identifier are left untouched because there
/// is no evidence that they are orphaned.
pub fn strip_orphan_tool_results(messages: &[Message]) -> Vec<Message> {
    let mut declared: HashSet<String> = HashSet::new();
    let mut result = Vec::with_capacity(messages.len());

    for message in messages {
        if message.role == Role::Assistant {
            for block in &message.content {
                if let ContentBlock::ToolCall(call) = block {
                    declared.insert(call.id.clone());
                }
            }
        }

        let had_results = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult(_)));
        let content: Vec<ContentBlock> = message
            .content
            .iter()
            .filter(|block| match block {
                ContentBlock::ToolResult(tool_result) => {
                    declared.contains(&tool_result.tool_call_id)
                }
                _ => true,
            })
            .cloned()
            .collect();

        // A tool-result message whose only payload was an orphan has no meaningful content.
        if message.role == Role::ToolResult && had_results && content.is_empty() {
            continue;
        }

        let mut kept = message.clone();
        kept.content = content;
        result.push(kept);
    }

    result
}

/// Ensures that every assistant tool call has a matching tool result.
///
/// For each unmatched call, a synthetic error result is inserted immediately after the
/// assistant message that requested it, in call order. A call identifier is synthesized
/// at most once, and a call that is answered anywhere in the transcript is left alone.
pub fn ensure_tool_results_for_calls(messages: &[Message]) -> Vec<Message> {
    let answered = result_ids(messages);
    let mut synthesized: HashSet<String> = HashSet::new();
    let mut result = Vec::with_capacity(messages.len());

    for message in messages {
        result.push(message.clone());
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.content {
            if let ContentBlock::ToolCall(call) = block {
                if answered.contains(&call.id) || !synthesized.insert(call.id.clone()) {
                    continue;
                }
                result.push(synthetic_result(call));
            }
        }
    }

    result
}

/// Whether a message consists exclusively of text blocks and carries no per-block state
/// that would be lost by concatenation.
fn is_plain_text(message: &Message) -> bool {
    message
        .content
        .iter()
        .all(|block| matches!(block, ContentBlock::Text { .. }))
}

/// Whether two neighboring messages can be folded together without losing information.
fn mergeable(left: &Message, right: &Message) -> bool {
    // Tool results are distinct executions even when their content happens to be text.
    if left.role == Role::ToolResult || right.role == Role::ToolResult {
        return false;
    }
    left.role == right.role
        && is_plain_text(left)
        && is_plain_text(right)
        && left.stop_reason == right.stop_reason
        && left.metadata == right.metadata
}

/// Folds `right` into `left`, preserving the earlier timestamp and combining usage.
fn merge_pair(left: &mut Message, right: Message) {
    let Message { content, usage, .. } = right;

    for block in content {
        if let ContentBlock::Text { text } = block {
            if let Some(ContentBlock::Text { text: previous }) = left.content.last_mut() {
                previous.push_str(&text);
            } else {
                left.content.push(ContentBlock::Text { text });
            }
        }
    }

    left.usage = match (left.usage.take(), usage) {
        (None, None) => None,
        (Some(usage), None) | (None, Some(usage)) => Some(usage),
        (Some(mut accumulated), Some(other)) => {
            accumulated.add_assign(&other);
            Some(accumulated)
        }
    };
}

/// Normalizes a transcript for cross-provider replay.
///
/// Orphans are stripped before results are synthesized so that a result appearing ahead
/// of its call cannot suppress the synthetic result needed after that call. Only after
/// both passes are adjacent, state-free text messages folded together.
pub fn normalize_transcript(messages: &[Message]) -> Vec<Message> {
    let paired = ensure_tool_results_for_calls(&strip_orphan_tool_results(messages));
    let mut result: Vec<Message> = Vec::with_capacity(paired.len());

    for message in paired {
        if let Some(last) = result.last_mut()
            && mergeable(last, &message)
        {
            merge_pair(last, message);
            continue;
        }
        result.push(message);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::{StopReason, Usage};
    use serde_json::json;

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text {
            text: value.to_owned(),
        }
    }

    fn call(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: json!({}),
        })
    }

    fn assistant(timestamp_ms: u64, content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            timestamp_ms,
            stop_reason: Some(StopReason::ToolUse),
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    fn tool_result(call_id: &str, name: &str, timestamp_ms: u64) -> Message {
        Message {
            role: Role::ToolResult,
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_call_id: call_id.to_owned(),
                tool_name: name.to_owned(),
                content: vec![text("ok")],
                is_error: false,
                details: None,
            })],
            timestamp_ms,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    fn result_id(message: &Message) -> &str {
        match &message.content[0] {
            ContentBlock::ToolResult(result) => &result.tool_call_id,
            _ => panic!("expected a tool result block"),
        }
    }

    #[test]
    fn empty_transcript_remains_empty() {
        assert!(normalize_transcript(&[]).is_empty());
        assert!(strip_orphan_tool_results(&[]).is_empty());
        assert!(ensure_tool_results_for_calls(&[]).is_empty());
    }

    #[test]
    fn strips_results_without_a_matching_call() {
        let messages = vec![
            assistant(1, vec![call("c1", "read")]),
            tool_result("c1", "read", 2),
            tool_result("c2", "write", 3),
        ];

        let stripped = strip_orphan_tool_results(&messages);

        assert_eq!(stripped.len(), 2);
        assert_eq!(result_id(&stripped[1]), "c1");
    }

    #[test]
    fn strips_results_that_precede_their_call() {
        let messages = vec![
            tool_result("c1", "read", 1),
            assistant(2, vec![call("c1", "read")]),
        ];

        let stripped = strip_orphan_tool_results(&messages);

        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].role, Role::Assistant);
    }

    #[test]
    fn inserts_synthetic_error_result_for_unanswered_call() {
        let messages = vec![assistant(1, vec![call("c1", "read")])];

        let normalized = ensure_tool_results_for_calls(&messages);

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[1].role, Role::ToolResult);
        assert_eq!(result_id(&normalized[1]), "c1");
        let ContentBlock::ToolResult(result) = &normalized[1].content[0] else {
            panic!("expected a tool result block");
        };
        assert!(result.is_error);
        assert_eq!(result.tool_name, "read");
        assert_eq!(result.content, vec![text("No result provided")]);
        assert!(normalized[1].timestamp_ms > 0);
    }

    #[test]
    fn does_not_duplicate_answered_calls() {
        let messages = vec![
            assistant(1, vec![call("c1", "read"), call("c2", "write")]),
            tool_result("c1", "read", 2),
            tool_result("c2", "write", 3),
        ];

        assert_eq!(ensure_tool_results_for_calls(&messages), messages);
    }

    #[test]
    fn closes_pending_calls_before_a_new_user_turn() {
        let messages = vec![
            assistant(1, vec![call("c1", "read")]),
            Message::user_text("next question", 2),
        ];

        let normalized = ensure_tool_results_for_calls(&messages);

        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0].role, Role::Assistant);
        assert_eq!(normalized[1].role, Role::ToolResult);
        assert_eq!(normalized[2].role, Role::User);
    }

    #[test]
    fn merges_adjacent_plain_text_messages() {
        let messages = vec![
            Message::user_text("hello ", 1),
            Message::user_text("world", 2),
            Message::assistant_text("hi", 3),
        ];

        let normalized = normalize_transcript(&messages);

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].role, Role::User);
        assert_eq!(normalized[0].text_content(), "hello world");
        assert_eq!(normalized[0].timestamp_ms, 1);
        assert_eq!(normalized[1].text_content(), "hi");
    }

    #[test]
    fn does_not_merge_across_tool_activity() {
        let messages = vec![
            Message::user_text("before", 1),
            assistant(2, vec![call("c1", "read")]),
            tool_result("c1", "read", 3),
            Message::user_text("after", 4),
        ];

        let normalized = normalize_transcript(&messages);

        assert_eq!(normalized.len(), 4);
        assert_eq!(normalized[0].text_content(), "before");
        assert_eq!(normalized[3].text_content(), "after");
    }

    #[test]
    fn does_not_merge_messages_with_distinct_metadata_or_stop_reason() {
        let mut second = Message::user_text("b", 2);
        second.metadata.insert("source".to_owned(), json!("test"));
        let messages = vec![Message::user_text("a", 1), second];

        let normalized = normalize_transcript(&messages);

        assert_eq!(normalized.len(), 2);

        let mut first = Message::assistant_text("one", 3);
        let mut other = Message::assistant_text("two", 4);
        other.stop_reason = Some(StopReason::Length);
        let messages = vec![first.clone(), other];

        let normalized = normalize_transcript(&messages);
        assert_eq!(normalized.len(), 2);

        first.stop_reason = None;
        let mut other = Message::assistant_text("two", 4);
        other.stop_reason = None;
        assert_eq!(normalize_transcript(&[first, other]).len(), 1);
    }

    #[test]
    fn merging_combines_usage() {
        let mut first = Message::user_text("a", 1);
        let mut second = Message::user_text("b", 2);
        first.usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        });
        second.usage = Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            ..Usage::default()
        });

        let normalized = normalize_transcript(&[first, second]);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].text_content(), "ab");
        let usage = normalized[0].usage.as_ref().expect("usage is preserved");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.total_tokens, 18);
    }

    #[test]
    fn normalize_replaces_orphan_result_with_synthetic_result() {
        let messages = vec![
            tool_result("c1", "read", 1),
            assistant(2, vec![text("checking"), call("c1", "read")]),
        ];

        let normalized = normalize_transcript(&messages);

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].role, Role::Assistant);
        assert_eq!(result_id(&normalized[1]), "c1");
        let ContentBlock::ToolResult(result) = &normalized[1].content[0] else {
            panic!("expected a tool result block");
        };
        assert!(result.is_error);
    }
}
