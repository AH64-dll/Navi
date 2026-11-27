//! Text and Unicode sanitization helpers shared by providers and transports.
//!
//! Rust strings are always valid UTF-8, so the surrogate cleanup performed by
//! the TypeScript original has no work to do here. What remains is the part
//! that keeps control bytes and terminal escape sequences out of text sent to
//! an API, plus UTF-16 helpers for code that must agree with JavaScript string
//! offsets.

/// Remove NUL bytes, disallowed control characters and complete ANSI CSI
/// escape sequences from `input`.
///
/// Tabs (`\t`), line feeds (`\n`) and carriage returns (`\r`) are preserved.
/// Escape sequences are removed first: stripping `ESC` as a control byte before
/// recognizing a CSI sequence would leave its parameters behind as plain text.
pub fn sanitize_text(input: &str) -> String {
    strip_ansi(input)
        .chars()
        .filter(|c| !is_disallowed_control(*c))
        .collect()
}

/// Remove complete ANSI CSI escape sequences from `input`.
///
/// A CSI sequence starts with `ESC [` or the equivalent C1 byte `U+009B` and
/// ends with a final byte in `0x40..=0x7E`, after optional parameter bytes
/// (`0x30..=0x3F`) and intermediate bytes (`0x20..=0x2F`). Incomplete sequences
/// and other escape forms (for example OSC or a bare `ESC`) are left intact.
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut copy_from = 0;
    let mut i = 0;

    while i < bytes.len() {
        // Both introducers are ASCII-range byte pairs, so scanning by byte
        // cannot split a multi-byte character: `0x1B` is ASCII, and `0xC2`
        // only leads the two-byte encoding of `U+0080..=U+00BF`.
        let introducer_len = match (bytes[i], bytes.get(i + 1).copied()) {
            (0x1b, Some(b'[')) | (0xc2, Some(0x9b)) => 2,
            _ => 0,
        };

        if introducer_len == 0 {
            i += 1;
            continue;
        }

        if let Some(end) = csi_end(bytes, i + introducer_len) {
            out.push_str(&input[copy_from..i]);
            i = end;
            copy_from = i;
            continue;
        }

        i += 1;
    }

    out.push_str(&input[copy_from..]);
    out
}

/// Truncate `input` to at most `max_chars` Unicode scalar values.
///
/// This counts Rust characters (code points), not UTF-8 bytes or UTF-16 code
/// units; a surrogate pair such as an emoji counts as one character here.
pub fn truncate_utf8_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

/// Number of UTF-16 code units in `s`, matching JavaScript's `s.length`.
///
/// Characters outside the Basic Multilingual Plane occupy two code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Slice `s` by UTF-16 code-unit offsets, matching JavaScript's `s.slice`.
///
/// Offsets outside the string are clamped and `start >= end` yields an empty
/// string. A character whose surrogate pair straddles either boundary is left
/// out entirely: a Rust `String` cannot hold half of a surrogate pair, so the
/// slice never returns invalid text.
pub fn utf16_slice(s: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }

    let mut out = String::new();
    let mut unit = 0;
    for ch in s.chars() {
        let next = unit + ch.len_utf16();
        if unit >= start && next <= end {
            out.push(ch);
        }
        unit = next;
        if unit >= end {
            break;
        }
    }
    out
}

/// Whether `c` is a control character that must not appear in sanitized text.
fn is_disallowed_control(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{8}'
            | '\u{b}'
            | '\u{c}'
            | '\u{e}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
    )
}

/// Index just past a CSI sequence whose introducer ends at `start`, or `None`
/// when there is no complete sequence at that position. Parameter bytes and
/// intermediate bytes are optional; the final byte is required.
fn csi_end(bytes: &[u8], mut start: usize) -> Option<usize> {
    while start < bytes.len() && (0x30..=0x3f).contains(&bytes[start]) {
        start += 1;
    }
    while start < bytes.len() && (0x20..=0x2f).contains(&bytes[start]) {
        start += 1;
    }
    if start < bytes.len() && (0x40..=0x7e).contains(&bytes[start]) {
        Some(start + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_text_removes_nul_and_control_chars_but_keeps_whitespace() {
        let input = "a\0b\u{1}c\u{8}d\u{b}e\u{c}f\u{e}g\u{1f}h\u{7f}i\u{9f}j\nk\rl\tm";
        assert_eq!(sanitize_text(input), "abcdefghij\nk\rl\tm");
        assert_eq!(sanitize_text("plain text"), "plain text");
        assert_eq!(sanitize_text(""), "");
    }

    #[test]
    fn sanitize_text_strips_csi_before_removing_escape_bytes() {
        assert_eq!(sanitize_text("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(
            sanitize_text("bold \u{1b}[1;32mgreen\u{1b}[0m end"),
            "bold green end"
        );
        assert_eq!(sanitize_text("\u{1b}[?25lhidden\u{1b}[?25h"), "hidden");
    }

    #[test]
    fn sanitize_text_removes_escape_from_incomplete_sequence() {
        // ESC is a disallowed control byte, so anything after an incomplete
        // introducer is preserved while the ESC itself is removed.
        assert_eq!(sanitize_text("\u{1b}[31"), "[31");
        assert_eq!(sanitize_text("\u{9b}"), "");
        assert_eq!(sanitize_text("a\u{1b}b"), "ab");
    }

    #[test]
    fn sanitize_text_preserves_valid_unicode() {
        let input = "Hello \u{1f648} World — 日本語";
        assert_eq!(sanitize_text(input), input);
        let combining = "e\u{301} cafe\u{301}";
        assert_eq!(sanitize_text(combining), combining);
    }

    #[test]
    fn sanitize_text_strips_c1_and_latin1_controls() {
        // C1 controls become empty in UTF-8 only through the C1 range, so the
        // filter must catch them after byte-level CSI stripping.
        assert_eq!(sanitize_text("\u{80}a\u{85}b\u{90}c\u{9f}d"), "abcd");
        // Non-control Latin-1 characters, including no-break space, stay.
        assert_eq!(sanitize_text("\u{a0}\u{a1}"), "\u{a0}\u{a1}");
    }

    #[test]
    fn sanitize_text_is_idempotent() {
        let input = "\u{1b}[1;31mred\u{1b}[0m\0 \u{85}x";
        let once = sanitize_text(input);
        assert_eq!(once, "red x");
        assert_eq!(sanitize_text(&once), once);
    }

    #[test]
    fn strip_ansi_removes_only_complete_csi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[2Jclear\u{1b}[H"), "clear");
        assert_eq!(strip_ansi("a\u{1b}[38;2;255;0;0mb\u{1b}[0mc"), "abc");
        assert_eq!(strip_ansi("\u{9b}31mred\u{9b}0m"), "red");
        assert_eq!(strip_ansi("\u{1b}[31"), "\u{1b}[31");
        assert_eq!(strip_ansi("\u{1b}plain"), "\u{1b}plain");
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
        assert_eq!(strip_ansi("\u{1b}[m"), "");
    }

    #[test]
    fn strip_ansi_handles_adjacent_and_intermediate_sequences() {
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[31mx\u{1b}[0m"), "x");
        assert_eq!(strip_ansi("a\u{1b}[!pb"), "ab");
        assert_eq!(strip_ansi("\u{1b}[38:2::255:0:0mcolored"), "colored");
        assert_eq!(strip_ansi("\u{9b}2Jclear"), "clear");
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[31m"), "");
    }

    #[test]
    fn strip_ansi_ignores_other_escape_forms() {
        // OSC, charset designation and a bare ESC are not CSI sequences.
        let osc = "\u{1b}]0;title\u{7}";
        assert_eq!(strip_ansi(osc), osc);
        let charset = "\u{1b}(B";
        assert_eq!(strip_ansi(charset), charset);
        assert_eq!(strip_ansi("\u{1b}"), "\u{1b}");
        assert_eq!(
            strip_ansi("\u{1b}[2J\u{1b}]8;;http\u{7}link"),
            "\u{1b}]8;;http\u{7}link"
        );
    }

    #[test]
    fn strip_ansi_requires_a_final_byte() {
        assert_eq!(strip_ansi("\u{1b}["), "\u{1b}[");
        assert_eq!(strip_ansi("\u{1b}[12;34"), "\u{1b}[12;34");
        assert_eq!(strip_ansi("\u{1b}[?"), "\u{1b}[?");
        assert_eq!(strip_ansi("\u{1b}[1 "), "\u{1b}[1 ");
        assert_eq!(strip_ansi("\u{9b}"), "\u{9b}");
        // A complete sequence followed by an incomplete one keeps the tail.
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[3"), "\u{1b}[3");
    }

    #[test]
    fn truncate_utf8_chars_counts_code_points_not_bytes_or_units() {
        assert_eq!(truncate_utf8_chars("hello world", 5), "hello");
        assert_eq!(truncate_utf8_chars("\u{1f600}ab", 1), "\u{1f600}");
        assert_eq!(truncate_utf8_chars("日本語", 2), "日本");
        assert_eq!(truncate_utf8_chars("abc", 0), "");
        assert_eq!(truncate_utf8_chars("abc", 10), "abc");
        assert_eq!(truncate_utf8_chars("", 1), "");
        // A combining mark is a separate code point, even though it renders
        // as part of the preceding grapheme.
        assert_eq!(truncate_utf8_chars("e\u{301}x", 1), "e");
    }

    #[test]
    fn truncate_utf8_chars_handles_empty_and_oversized_limits() {
        assert_eq!(truncate_utf8_chars("", 0), "");
        assert_eq!(truncate_utf8_chars("", usize::MAX), "");
        assert_eq!(truncate_utf8_chars("abc", 3), "abc");
        assert_eq!(truncate_utf8_chars("abc", usize::MAX), "abc");
    }

    #[test]
    fn utf16_len_matches_javascript_string_length() {
        assert_eq!(utf16_len(""), 0);
        assert_eq!(utf16_len("hello"), 5);
        assert_eq!(utf16_len("🙂"), 2);
        assert_eq!(utf16_len("a🙂b"), 4);
        assert_eq!(utf16_len("日本語"), 3);
        // A combining mark is one UTF-16 code unit.
        assert_eq!(utf16_len("e\u{301}"), 2);
    }

    #[test]
    fn utf16_slice_clamps_offsets_and_never_splits_a_surrogate_pair() {
        let text = "a🙂b";
        assert_eq!(utf16_slice(text, 0, 4), text);
        assert_eq!(utf16_slice(text, 0, 2), "a");
        assert_eq!(utf16_slice(text, 1, 3), "🙂");
        assert_eq!(utf16_slice(text, 2, 4), "b");
        assert_eq!(utf16_slice(text, 1, 2), "");
        assert_eq!(utf16_slice(text, 3, 3), "");
        assert_eq!(utf16_slice(text, 5, 9), "");
        assert_eq!(utf16_slice(text, 0, 99), text);
        assert_eq!(utf16_slice(text, 3, 1), "");
        assert_eq!(utf16_slice(text, 0, 0), "");
        assert_eq!(utf16_slice("hello world", 6, 11), "world");
        assert_eq!(utf16_slice("", 0, 1), "");
    }

    #[test]
    fn utf16_slice_composes_with_utf16_len() {
        let text = "x🙂日本語🙂y";
        let total = utf16_len(text);
        assert_eq!(utf16_slice(text, 0, total), text);
        assert_eq!(utf16_slice(text, total, total), "");
        assert_eq!(utf16_slice(text, 1, total - 1), "🙂日本語🙂");
    }
}
