//! Incremental parser for Server-Sent Events (SSE) streams.
//!
//! Lines end with LF, CRLF, or a bare CR, and a blank line dispatches the
//! current event. Only the incomplete last line is buffered between `push`
//! calls, so parsing a large event split across chunks stays linear in size.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// One dispatched server-sent event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SseEvent {
    /// Value of the `event:` field, if the block contained one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// All `data:` values joined by `\n` in arrival order.
    pub data: String,
    /// Value of the `id:` field in this block, if it was present and valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Reconnection delay from `retry:` in milliseconds, if it was numeric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u64>,
}

/// Text payloads that can be checked for the `[DONE]` sentinel.
///
/// Implemented for [`SseEvent`] and for string-like values so callers can test
/// either a parsed event or a raw data line.
pub trait DoneData {
    /// Returns the text compared against the sentinel.
    fn done_data(&self) -> &str;
}

impl DoneData for SseEvent {
    fn done_data(&self) -> &str {
        &self.data
    }
}

impl DoneData for str {
    fn done_data(&self) -> &str {
        self
    }
}

impl DoneData for String {
    fn done_data(&self) -> &str {
        self.as_str()
    }
}

impl<T: DoneData + ?Sized> DoneData for &T {
    fn done_data(&self) -> &str {
        T::done_data(*self)
    }
}

/// Returns whether the payload is exactly `[DONE]`, ignoring surrounding whitespace.
pub fn is_done_event<T: DoneData>(value: T) -> bool {
    value.done_data().trim() == "[DONE]"
}

/// Incremental SSE parser.
///
/// Feed it text with [`SseParser::push`] and call [`SseParser::finish`] once the
/// stream ends to dispatch a final event that had no terminating blank line.
#[derive(Debug, Default)]
pub struct SseParser {
    /// The trailing line that has not been terminated yet.
    pending_line: String,
    /// The previous chunk ended with CR; a leading LF now completes that CRLF.
    skip_leading_lf: bool,
    event: Option<String>,
    data_lines: Vec<String>,
    id: Option<String>,
    retry: Option<u64>,
}

impl SseParser {
    /// Creates a parser with no buffered input or event state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds the next text chunk and returns the events it completed.
    ///
    /// CR and LF both end a line, and an LF immediately after a CR is treated
    /// as the second half of one CRLF, including when split across chunks.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let bytes = chunk.as_bytes();
        let mut start = 0usize;
        let mut i = 0usize;

        if self.skip_leading_lf && bytes.first() == Some(&b'\n') {
            start = 1;
            i = 1;
        }
        if !bytes.is_empty() {
            self.skip_leading_lf = false;
        }

        // CR and LF are single ASCII bytes, never part of a multi-byte UTF-8
        // sequence, so every slice boundary below is a character boundary.
        while i < bytes.len() {
            let byte = bytes[i];
            if byte != b'\r' && byte != b'\n' {
                i += 1;
                continue;
            }

            let line = self.take_line(&chunk[start..i]);
            if let Some(event) = self.decode_line(line.as_ref()) {
                events.push(event);
            }

            i += 1;
            if byte == b'\r' {
                if i == bytes.len() {
                    self.skip_leading_lf = true;
                } else if bytes[i] == b'\n' {
                    i += 1;
                }
            }
            start = i;
        }

        self.pending_line.push_str(&chunk[start..]);
        events
    }

    /// Ends the stream and returns a final unterminated event, if any.
    ///
    /// All parser state is cleared, so the same parser can be reused.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.pending_line.is_empty() {
            // A non-empty line cannot dispatch an event; only a blank line can.
            let line = std::mem::take(&mut self.pending_line);
            let _ = self.decode_line(&line);
        }
        self.skip_leading_lf = false;
        let event = self.flush();
        self.event = None;
        self.data_lines.clear();
        self.id = None;
        self.retry = None;
        event
    }

    /// Returns the complete line formed by the pending prefix and `segment`.
    fn take_line<'a>(&mut self, segment: &'a str) -> Cow<'a, str> {
        if self.pending_line.is_empty() {
            Cow::Borrowed(segment)
        } else {
            let mut line = std::mem::take(&mut self.pending_line);
            line.push_str(segment);
            Cow::Owned(line)
        }
    }

    /// Applies one complete line, dispatching an event when the line is blank.
    fn decode_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            return None;
        }

        let (field, raw_value) = match line.split_once(':') {
            Some((field, value)) => (field, value),
            None => (line, ""),
        };
        let value = raw_value.strip_prefix(' ').unwrap_or(raw_value);

        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data_lines.push(value.to_owned()),
            // The SSE format reserves NUL in an id: ignore such fields.
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            // A non-numeric retry value is ignored rather than failing the stream.
            "retry" => {
                if let Ok(millis) = value.parse::<u64>() {
                    self.retry = Some(millis);
                }
            }
            _ => {}
        }
        None
    }

    /// Dispatches the buffered block when it carries an event name or data.
    fn flush(&mut self) -> Option<SseEvent> {
        let has_event = self.event.as_ref().is_some_and(|name| !name.is_empty());
        if self.data_lines.is_empty() && !has_event {
            self.event = None;
            return None;
        }

        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data_lines).join("\n"),
            id: self.id.take(),
            retry: self.retry.take(),
        })
    }
}

/// Parses a complete SSE stream, including a final event without a blank line.
pub fn parse_sse_stream(input: &str) -> Vec<SseEvent> {
    let mut parser = SseParser::new();
    let mut events = parser.push(input);
    if let Some(last) = parser.finish() {
        events.push(last);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_fields_from_one_event() {
        let events =
            parse_sse_stream("event: message\r\nid: 7\r\nretry: 1500\r\ndata: hello\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[0].retry, Some(1500));
    }

    #[test]
    fn joins_data_lines_and_strips_only_one_leading_space() {
        let events = parse_sse_stream("data: one\ndata:  two\ndata:three\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one\n two\nthree");
        assert_eq!(events[0].event, None);
    }

    #[test]
    fn accepts_lf_crlf_and_bare_cr_line_endings() {
        let events = parse_sse_stream("data: a\rdata: b\r\ndata: c\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb\nc");
    }

    #[test]
    fn incremental_push_handles_split_lines_and_split_crlf() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: hel").is_empty());
        assert!(parser.push("lo\r").is_empty());
        assert!(parser.push("\ndata: world\n").is_empty());
        let events = parser.push("\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello\nworld");
    }

    #[test]
    fn one_chunk_can_complete_several_events() {
        let mut parser = SseParser::new();
        let events = parser.push("data: a\n\ndata: b\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "a");
        assert_eq!(events[1].data, "b");
    }

    #[test]
    fn ignores_comments_unknown_fields_and_blocks_without_content() {
        let events = parse_sse_stream(": keep-alive\nfoo: bar\n\n\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn finish_dispatches_unterminated_event_and_resets_parser() {
        let mut parser = SseParser::new();
        assert!(parser.push("event: ping\ndata: 42").is_empty());
        let event = parser.finish().expect("unterminated event");
        assert_eq!(event.event.as_deref(), Some("ping"));
        assert_eq!(event.data, "42");
        assert!(parser.finish().is_none());
        let fresh = parser.push("data: fresh\n\n");
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].data, "fresh");
    }

    #[test]
    fn invalid_retry_and_nul_id_are_ignored() {
        let events = parse_sse_stream("retry: later\nid: bad\0id\ndata: ok\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "ok");
        assert_eq!(events[0].retry, None);
        assert_eq!(events[0].id, None);
    }

    #[test]
    fn detects_done_sentinel_on_events_and_raw_payloads() {
        let events = parse_sse_stream("data: [DONE]\n\n");
        assert!(is_done_event(&events[0]));
        assert!(is_done_event("[DONE]"));
        assert!(is_done_event(String::from(" [DONE] ")));
        let other = parse_sse_stream("data: still going\n\n");
        assert!(!is_done_event(&other[0]));
    }

    #[test]
    fn empty_input_and_empty_chunks_produce_no_events() {
        assert!(parse_sse_stream("").is_empty());
        let mut parser = SseParser::new();
        assert!(parser.push("").is_empty());
        assert!(parser.finish().is_none());
    }

    #[test]
    fn event_without_data_dispatches_but_retry_only_block_does_not() {
        let events = parse_sse_stream("event: ping\n\nretry: 10\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn multibyte_characters_survive_chunk_boundaries() {
        let mut parser = SseParser::new();
        assert!(parser.push("data: \u{e9}").is_empty());
        let events = parser.push("\u{e0}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "\u{e9}\u{e0}");
    }
}
