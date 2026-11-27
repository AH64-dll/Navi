//! Length-prefixed Native Messaging frames and newline-delimited JSON framing.
//!
//! Chrome Native Messaging prefixes each message with a four-byte little-endian length. The local
//! socket hop uses the same framing; both peers of each hop run on the same host, so the byte order
//! is unambiguous.
//!
//! A decoder that fails is terminal: its partial state is discarded and every later call returns
//! the stored error. Callers must disconnect instead of resynchronising on the byte stream.

use navi_types::{NaviError, NaviResult};

/// Default upper bound for one frame body, matching `MAX_ENVELOPE_BYTES` in the TypeScript protocol.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024;

/// Size of the Native Messaging length header.
const HEADER_BYTES: usize = 4;

/// Encodes one payload as a four-byte little-endian length followed by the payload bytes.
///
/// Empty payloads, payloads above `max_bytes`, and payloads too large for a 32-bit length are
/// rejected. JSON serialisation and validation belong to the caller: this function frames bytes.
pub fn encode_native_message(payload: &[u8], max_bytes: usize) -> NaviResult<Vec<u8>> {
    let len = payload.len();
    if len == 0 {
        return Err(NaviError::InvalidInput(
            "frame payload must not be empty".to_owned(),
        ));
    }
    if len > max_bytes {
        return Err(NaviError::InvalidInput(format!(
            "frame body is {len} bytes, above the {max_bytes} byte cap"
        )));
    }
    let length = u32::try_from(len).map_err(|_| {
        NaviError::InvalidInput(format!(
            "frame body of {len} bytes does not fit a 32-bit length header"
        ))
    })?;

    let mut frame = Vec::with_capacity(HEADER_BYTES + len);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Incremental reader for little-endian Native Messaging frames.
///
/// `push` returns every complete frame body in a chunk and tolerates arbitrary fragmentation,
/// coalescing and one-byte chunks. Wire failures are permanent: once failed, the decoder holds no
/// partial state and rejects all further input.
#[derive(Debug)]
pub struct NativeMessageDecoder {
    max_bytes: usize,
    header: [u8; HEADER_BYTES],
    header_len: usize,
    body: Vec<u8>,
    expected_len: Option<usize>,
    failure: Option<NaviError>,
    finished: bool,
}

impl NativeMessageDecoder {
    /// Creates a decoder that rejects declared bodies longer than `max_bytes`.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            header: [0; HEADER_BYTES],
            header_len: 0,
            body: Vec::new(),
            expected_len: None,
            failure: None,
            finished: false,
        }
    }

    /// Creates a decoder using [`DEFAULT_MAX_FRAME_BYTES`] as the body cap.
    pub fn with_default_limit() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }

    /// Bytes buffered for the current partial frame; zero when aligned on a header.
    pub fn pending_bytes(&self) -> usize {
        self.header_len + self.body.len()
    }

    /// Whether a previous call failed permanently.
    pub fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// Consumes a chunk and returns every complete frame body it contains.
    ///
    /// A framing error discards frames decoded earlier in the same chunk and fails the decoder.
    pub fn push(&mut self, bytes: &[u8]) -> NaviResult<Vec<Vec<u8>>> {
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }
        if self.finished {
            return Err(NaviError::Protocol(
                "frame decoder is already finished".to_owned(),
            ));
        }

        let mut frames = Vec::new();
        let result = self.push_chunk(bytes, &mut frames);
        if let Err(err) = &result {
            self.fail(err.clone());
        }
        result.map(|()| frames)
    }

    /// Declares end of input. A partial header or body is a protocol error, not a short frame.
    pub fn finish(&mut self) -> NaviResult<()> {
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }
        if self.finished {
            return Err(NaviError::Protocol(
                "frame decoder is already finished".to_owned(),
            ));
        }
        self.finished = true;

        // In the body phase the header is already consumed, so the whole declared body counts as
        // an incomplete frame even though only some of its bytes are buffered.
        let partial = match self.expected_len {
            Some(len) => len,
            None => self.header_len,
        };
        if partial != 0 {
            let err = NaviError::Protocol(format!(
                "input ended with {partial} bytes of a partial frame"
            ));
            self.fail(err.clone());
            return Err(err);
        }
        Ok(())
    }

    fn push_chunk(&mut self, bytes: &[u8], frames: &mut Vec<Vec<u8>>) -> NaviResult<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let len = match self.expected_len {
                Some(len) => len,
                None => {
                    let take = (HEADER_BYTES - self.header_len).min(bytes.len() - offset);
                    self.header[self.header_len..self.header_len + take]
                        .copy_from_slice(&bytes[offset..offset + take]);
                    self.header_len += take;
                    offset += take;
                    if self.header_len < HEADER_BYTES {
                        continue;
                    }
                    self.header_len = 0;

                    let len = u32::from_le_bytes(self.header) as usize;
                    if len == 0 {
                        return Err(NaviError::Protocol(
                            "frame length must be positive".to_owned(),
                        ));
                    }
                    if len > self.max_bytes {
                        return Err(NaviError::Protocol(format!(
                            "declared frame length {len} exceeds the {} byte cap",
                            self.max_bytes
                        )));
                    }
                    self.body.clear();
                    self.expected_len = Some(len);
                    len
                }
            };

            let take = (len - self.body.len()).min(bytes.len() - offset);
            self.body.extend_from_slice(&bytes[offset..offset + take]);
            offset += take;
            if self.body.len() < len {
                continue;
            }

            self.expected_len = None;
            frames.push(std::mem::take(&mut self.body));
        }
        Ok(())
    }

    fn fail(&mut self, err: NaviError) {
        self.failure = Some(err);
        self.header_len = 0;
        self.body.clear();
        self.expected_len = None;
    }
}

/// Incremental framer for newline-delimited JSON.
///
/// Lines are returned as raw bytes without their terminating newline; CRLF is accepted and the
/// carriage return is stripped. The final line need not be newline-terminated and is returned by
/// `finish`. Parsing the JSON is left to the caller.
#[derive(Debug)]
pub struct NdjsonFramer {
    max_bytes: usize,
    line: Vec<u8>,
    failure: Option<NaviError>,
    finished: bool,
}

impl NdjsonFramer {
    /// Creates a framer that rejects lines longer than `max_bytes`, excluding the newline.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            line: Vec::new(),
            failure: None,
            finished: false,
        }
    }

    /// Creates a framer using [`DEFAULT_MAX_FRAME_BYTES`] as the line cap.
    pub fn with_default_limit() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }

    /// Bytes buffered for the current partial line.
    pub fn pending_bytes(&self) -> usize {
        self.line.len()
    }

    /// Whether a previous call failed permanently.
    pub fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// Consumes a chunk and returns every newline-terminated line it completes.
    pub fn push(&mut self, bytes: &[u8]) -> NaviResult<Vec<Vec<u8>>> {
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }
        if self.finished {
            return Err(NaviError::Protocol(
                "NDJSON framer is already finished".to_owned(),
            ));
        }

        let mut lines = Vec::new();
        let result = self.push_chunk(bytes, &mut lines);
        if let Err(err) = &result {
            self.fail(err.clone());
        }
        result.map(|()| lines)
    }

    /// Returns the unterminated final line, if any. An empty buffer yields `None`.
    pub fn finish(&mut self) -> NaviResult<Option<Vec<u8>>> {
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }
        if self.finished {
            return Err(NaviError::Protocol(
                "NDJSON framer is already finished".to_owned(),
            ));
        }
        self.finished = true;
        if self.line.is_empty() {
            return Ok(None);
        }
        let mut line = std::mem::take(&mut self.line);
        strip_carriage_return(&mut line);
        Ok(Some(line))
    }

    fn push_chunk(&mut self, bytes: &[u8], lines: &mut Vec<Vec<u8>>) -> NaviResult<()> {
        for &byte in bytes {
            if byte == b'\n' {
                // The buffer already holds every byte of this line from earlier chunks plus the
                // bytes seen before this newline, so it is complete as-is.
                let mut line = std::mem::take(&mut self.line);
                strip_carriage_return(&mut line);
                lines.push(line);
                continue;
            }
            self.line.push(byte);
            if self.line.len() > self.max_bytes {
                return Err(NaviError::Protocol(format!(
                    "NDJSON line exceeds the {} byte cap",
                    self.max_bytes
                )));
            }
        }
        Ok(())
    }

    fn fail(&mut self, err: NaviError) {
        self.failure = Some(err);
        self.line.clear();
    }
}

fn strip_carriage_return(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAX_FRAME_BYTES, NativeMessageDecoder, NdjsonFramer, encode_native_message,
    };
    use serde_json::json;

    fn frame(body: &[u8]) -> Vec<u8> {
        encode_native_message(body, DEFAULT_MAX_FRAME_BYTES).expect("test frame")
    }

    #[test]
    fn encode_writes_four_byte_little_endian_header() {
        let body = br#"{"type":"ping","id":7}"#;
        let encoded = encode_native_message(body, DEFAULT_MAX_FRAME_BYTES).expect("encode");
        let len_bytes = (body.len() as u32).to_le_bytes();

        assert_eq!(encoded.len(), 4 + body.len());
        assert_eq!(&encoded[..4], &len_bytes);
        assert_eq!(&encoded[4..], body);
        // The header is little-endian, not the reverse order.
        assert_ne!(&encoded[..4], &(body.len() as u32).to_be_bytes());
    }

    #[test]
    fn encode_rejects_empty_and_oversized_payloads() {
        assert!(encode_native_message(b"", DEFAULT_MAX_FRAME_BYTES).is_err());

        let at_limit = vec![b'x'; DEFAULT_MAX_FRAME_BYTES];
        let above_limit = vec![b'x'; DEFAULT_MAX_FRAME_BYTES + 1];
        assert!(encode_native_message(&at_limit, DEFAULT_MAX_FRAME_BYTES).is_ok());
        assert!(encode_native_message(&above_limit, DEFAULT_MAX_FRAME_BYTES).is_err());
        assert!(encode_native_message(b"abc", 2).is_err());
    }

    #[test]
    fn decoder_accepts_one_byte_fragments() {
        let mut decoder = NativeMessageDecoder::new(1024);
        let stream = [frame(b"first"), frame(br#"{"n":2}"#)].concat();
        let mut decoded = Vec::new();

        for (index, byte) in stream.iter().enumerate() {
            let frames = decoder.push(&[*byte]).expect("byte chunk");
            decoded.extend(frames);
            if index % 3 == 0 {
                assert!(decoder.pending_bytes() <= 4 + 1024);
            }
        }

        assert_eq!(decoded, vec![b"first".to_vec(), br#"{"n":2}"#.to_vec()]);
        assert_eq!(decoder.pending_bytes(), 0);
        decoder.finish().expect("aligned decoder");
    }

    #[test]
    fn decoder_accepts_coalesced_frames_and_empty_chunks() {
        let mut decoder = NativeMessageDecoder::new(1024);
        let stream = [frame(b"a"), frame(b"bb"), frame(b"ccc")].concat();

        assert!(decoder.push(&[]).expect("empty chunk").is_empty());
        let frames = decoder.push(&stream).expect("coalesced chunks");
        assert_eq!(frames, vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]);
        assert!(decoder.push(&[]).expect("empty chunk").is_empty());
        decoder.finish().expect("aligned decoder");
    }

    #[test]
    fn decoder_roundtrips_serialized_json() {
        let value = json!({"kind": "event", "values": [1, 2, 3]});
        let body = serde_json::to_vec(&value).expect("serialize");
        let encoded = encode_native_message(&body, DEFAULT_MAX_FRAME_BYTES).expect("encode");

        let mut decoder = NativeMessageDecoder::new(DEFAULT_MAX_FRAME_BYTES);
        let frames = decoder.push(&encoded).expect("decode");
        assert_eq!(frames, vec![body.clone()]);

        let parsed: serde_json::Value = serde_json::from_slice(&frames[0]).expect("parse");
        assert_eq!(parsed, value);
    }

    #[test]
    fn decoder_rejects_bad_lengths_and_stays_failed() {
        let mut zero = NativeMessageDecoder::new(1024);
        assert!(zero.push(&[0, 0, 0, 0]).is_err());
        assert!(zero.failed());
        assert_eq!(zero.pending_bytes(), 0);
        assert!(zero.push(&frame(b"later")).is_err());
        assert!(zero.finish().is_err());

        let too_large = (1025_u32).to_le_bytes();
        let mut oversized = NativeMessageDecoder::new(1024);
        assert!(oversized.push(&too_large).is_err());
        assert!(oversized.failed());
        assert!(oversized.push(&too_large).is_err());
    }

    #[test]
    fn decoder_finish_detects_partial_header_and_body() {
        let mut header = NativeMessageDecoder::new(1024);
        header.push(&[1, 0]).expect("partial header");
        assert_eq!(header.pending_bytes(), 2);
        assert!(header.finish().is_err());

        let mut body = NativeMessageDecoder::new(1024);
        let encoded = frame(b"hello");
        body.push(&encoded[..6]).expect("partial body");
        assert_eq!(body.pending_bytes(), 2);
        assert!(body.finish().is_err());
        assert!(body.push(&encoded[6..]).is_err());
    }

    #[test]
    fn ndjson_framer_splits_lines_across_chunks() {
        let mut framer = NdjsonFramer::new(1024);
        let mut lines = Vec::new();

        lines.extend(framer.push(b"{\"a\":1}\r\n{\"b\":").expect("first chunk"));
        lines.extend(framer.push(b"2}\n{\"c\":3}\n").expect("second chunk"));

        assert_eq!(
            lines,
            vec![
                br#"{"a":1}"#.to_vec(),
                br#"{"b":2}"#.to_vec(),
                br#"{"c":3}"#.to_vec(),
            ]
        );
        assert_eq!(framer.pending_bytes(), 0);
        assert!(framer.finish().expect("finish").is_none());
    }

    #[test]
    fn ndjson_framer_rejects_overlong_line_and_returns_tail() {
        let mut framer = NdjsonFramer::new(4);
        assert!(framer.push(b"12345\n").is_err());
        assert!(framer.failed());
        assert_eq!(framer.pending_bytes(), 0);
        assert!(framer.push(b"1\n").is_err());

        let mut tail = NdjsonFramer::new(16);
        assert_eq!(tail.push(b"one\ntw").expect("chunk"), vec![b"one".to_vec()]);
        assert_eq!(tail.pending_bytes(), 2);
        assert_eq!(tail.finish().expect("finish"), Some(b"tw".to_vec()));
        assert!(tail.finish().is_err());
    }

    #[test]
    fn decoder_preserves_arbitrary_bytes_without_parsing() {
        let body = [0xff, 0x00, 0x10, b'{', b'}', 0x80];
        let mut decoder = NativeMessageDecoder::new(1024);
        let frames = decoder.push(&frame(&body)).expect("decode raw bytes");
        assert_eq!(frames, vec![body.to_vec()]);
        assert_eq!(decoder.pending_bytes(), 0);
        decoder.finish().expect("aligned decoder");
    }

    #[test]
    fn ndjson_framer_preserves_empty_lines_and_embedded_carriage_returns() {
        let mut framer = NdjsonFramer::new(64);
        let lines = framer
            .push(b"\r\n\n{\"text\":\"a\rb\"}\r\n")
            .expect("chunk");
        assert_eq!(
            lines,
            vec![Vec::new(), Vec::new(), b"{\"text\":\"a\rb\"}".to_vec()]
        );
        assert_eq!(framer.pending_bytes(), 0);
        assert!(framer.finish().expect("finish").is_none());
    }

    #[test]
    fn ndjson_framer_enforces_cap_at_boundary_and_across_chunks() {
        let mut at_cap = NdjsonFramer::new(4);
        let lines = at_cap.push(b"1234\n").expect("line exactly at cap");
        assert_eq!(lines, vec![b"1234".to_vec()]);

        let mut across = NdjsonFramer::new(4);
        assert!(across.push(b"12").expect("within cap").is_empty());
        assert_eq!(across.pending_bytes(), 2);
        assert!(across.push(b"345").is_err());
        assert!(across.failed());
        assert_eq!(across.pending_bytes(), 0);
        assert!(across.push(b"1\n").is_err());
        assert!(across.finish().is_err());
    }

    #[test]
    fn ndjson_framer_strips_cr_split_across_chunks() {
        let mut framer = NdjsonFramer::new(64);
        assert!(framer.push(b"{\"a\":1}\r").expect("first chunk").is_empty());
        let lines = framer.push(b"\n{\"b\":2}\r\n").expect("second chunk");

        assert_eq!(lines, vec![br#"{"a":1}"#.to_vec(), br#"{"b":2}"#.to_vec()]);
        assert_eq!(framer.pending_bytes(), 0);
        assert!(framer.finish().expect("finish").is_none());
    }

    #[test]
    fn ndjson_framer_finish_strips_trailing_carriage_return() {
        let mut framer = NdjsonFramer::new(16);
        assert!(framer.push(b"tail\r").expect("chunk").is_empty());
        assert_eq!(framer.finish().expect("finish"), Some(b"tail".to_vec()));
    }

    #[test]
    fn ndjson_framer_returns_earlier_lines_except_on_error() {
        let mut framer = NdjsonFramer::new(4);
        // The overlong line errors the framer, and the lines decoded before it are discarded.
        assert!(framer.push(b"ab\n12345\n").is_err());
        assert!(framer.failed());
        assert_eq!(framer.pending_bytes(), 0);
    }
}
