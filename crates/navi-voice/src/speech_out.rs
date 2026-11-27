//! Speech-output planning for text-to-speech playback.
//!
//! Ported from `apps/navi/workers/speech_out.py` and the text-cleaning helpers
//! in `apps/navi/workers/persona.py`. The full sanitized reply stays in the
//! transcript; only its first sentences are handed to the voice loop. No
//! external regex crate is used: each rewrite is a small scanner over `str`.

use serde::{Deserialize, Serialize};

/// Hard cap on spoken characters, matching `persona.first_sentences(maximum=600)`.
const MAX_SPOKEN_CHARS: usize = 600;

/// Sentence-ending abbreviations that must not split a sentence.
const SHORT_FORMS: [&str; 10] = [
    "e.g.", "i.e.", "etc.", "vs.", "mr.", "mrs.", "ms.", "dr.", "no.", "approx.",
];

/// Describes how an assistant reply should be spoken by a TTS backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechOutputPlan {
    /// Resolved persona name.
    pub persona: String,
    /// TTS voice identifier.
    pub voice: String,
    /// Speech rate adjustment (for example `+0%`).
    pub rate: String,
    /// Full sanitized text for the transcript and UI.
    pub text: String,
    /// Shortened text that should actually be spoken.
    pub spoken_text: String,
    /// Whether sentences or characters were dropped for playback.
    pub truncated: bool,
}

/// Builds a speech plan from a raw assistant reply and persona hints.
pub fn plan_speech_output(
    text: &str,
    persona: &str,
    voice: &str,
    rate: &str,
    max_sentences: usize,
) -> SpeechOutputPlan {
    let utterance = sanitize_for_tts(text);
    let (spoken_text, truncated) = spoken_and_truncated(&utterance, max_sentences);
    SpeechOutputPlan {
        persona: persona.to_string(),
        voice: voice.to_string(),
        rate: rate.to_string(),
        text: utterance,
        spoken_text,
        truncated,
    }
}

/// Strips markdown and code so spoken output is clean prose.
pub fn sanitize_for_tts(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = strip_bookkeeping_lines(text);
    out = strip_fenced_code(&out);
    out = strip_inline_code(&out);
    out = strip_headings(&out);
    out = strip_star_emphasis(&out);
    out = strip_delimited_pair(&out, "__", '_', false);
    out = strip_delimited_pair(&out, "~~", '~', false);
    out = strip_bullets(&out);
    out = strip_numbering(&out);
    out = substitute_bracketed(&out, "[", false, |label| label);
    out = substitute_bracketed(&out, "![", true, |_| " ");
    out = strip_quotes(&out);
    out = strip_rules(&out);
    out = out.replace('|', ", ");
    out = replace_urls(&out);
    out = shorten_paths(&out);
    out = strip_emoji(&out);
    out = collapse_spaces(&out);
    out = join_paragraphs(&out);
    out = join_soft_lines(&out);
    out.trim().to_string()
}

/// Splits text at punctuation followed by whitespace or the end of input.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0;
    for (punct_start, end) in sentence_end_spans(text) {
        let last_word = text[start..punct_start + 1]
            .split_whitespace()
            .next_back()
            .unwrap_or_default()
            .to_lowercase();
        if SHORT_FORMS.contains(&last_word.as_str()) {
            continue;
        }
        sentences.push(text[start..end].trim().to_string());
        start = end;
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        sentences.push(rest.to_string());
    }
    sentences
}

/// Returns clean prose capped to `max_sentences` sentences and 600 characters.
pub fn first_sentences(text: &str, max_sentences: usize) -> String {
    let cleaned = sanitize_for_tts(text);
    spoken_and_truncated(&cleaned, max_sentences).0
}

/// Returns the spoken text and whether sentences or characters were dropped.
fn spoken_and_truncated(cleaned: &str, max_sentences: usize) -> (String, bool) {
    if cleaned.is_empty() {
        return (String::new(), false);
    }
    let sentences = split_sentences(cleaned);
    let mut spoken = sentences
        .iter()
        .take(max_sentences)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    spoken = spoken.trim().to_string();
    let mut truncated = sentences.len() > max_sentences;
    if spoken.chars().count() > MAX_SPOKEN_CHARS {
        spoken = truncate_on_word(&spoken, MAX_SPOKEN_CHARS);
        truncated = true;
    }
    (spoken, truncated)
}

/// Cuts a long utterance after the last whole word that fits, then adds a dot.
fn truncate_on_word(spoken: &str, max_chars: usize) -> String {
    let prefix: String = spoken.chars().take(max_chars).collect();
    let end = prefix.rfind(' ').unwrap_or(prefix.len());
    let words = prefix[..end].trim_end_matches([' ', ',', ';', ':']);
    let mut out = String::with_capacity(words.len() + 1);
    out.push_str(words);
    out.push('.');
    out
}

/// Returns the `char` at a byte offset, or `None` when the offset is invalid.
fn next_char(text: &str, at: usize) -> Option<char> {
    text.get(at..)?.chars().next()
}

/// Consumes the longest run of whitespace starting at an offset.
fn skip_whitespace(text: &str, at: usize) -> usize {
    let mut pos = at;
    while let Some(c) = next_char(text, pos) {
        if !c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// Applies a line-anchored rewrite whose leading whitespace may span blank lines.
///
/// `try_match` receives a line start and returns the end of the match, if any.
fn transform_line_prefixes(
    text: &str,
    mut try_match: impl FnMut(&str, usize) -> Option<usize>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut line_start = 0;
    loop {
        let Some(end) = try_match(text, line_start) else {
            match text[line_start..].find('\n') {
                Some(n) => line_start += n + 1,
                None => break,
            }
            continue;
        };
        if end > line_start {
            out.push_str(&text[copied..line_start]);
            copied = end;
        }
        if end >= text.len() {
            break;
        }
        line_start = if end == 0 || text.as_bytes()[end - 1] == b'\n' {
            end
        } else {
            match text[end..].find('\n') {
                Some(n) => end + n + 1,
                None => break,
            }
        };
    }
    out.push_str(&text[copied..]);
    out
}

/// Drops Navi's own `PROGRESS` and `TASK` bookkeeping lines.
fn strip_bookkeeping_lines(text: &str) -> String {
    transform_line_prefixes(text, |text, line_start| {
        let q = skip_whitespace(text, line_start);
        for keyword in ["PROGRESS", "TASK"] {
            let Some(after) = text[q..].strip_prefix(keyword) else {
                continue;
            };
            let Some(separator) = after.chars().next() else {
                continue;
            };
            if !separator.is_whitespace() {
                continue;
            }
            let mut end = q + keyword.len() + separator.len_utf8();
            while let Some(c) = next_char(text, end) {
                if c == '\n' {
                    break;
                }
                end += c.len_utf8();
            }
            return Some(end);
        }
        None
    })
}

/// Removes fenced code blocks, including an unterminated trailing fence.
fn strip_fenced_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(open) = rest.find("```") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        out.push(' ');
        match rest[open + 3..].find("```") {
            Some(close) => rest = &rest[open + 3 + close + 3..],
            None => break,
        }
    }
    out
}

/// Replaces backtick-quoted spans with their unquoted content.
fn strip_inline_code(text: &str) -> String {
    strip_delimited_pair(text, "`", '`', true)
}

/// Removes line prefixes made of up to three spaces and one to six hashes.
fn strip_headings(text: &str) -> String {
    transform_line_prefixes(text, |text, line_start| {
        let mut q = line_start;
        let mut leading = 0;
        while leading < 3 {
            let Some(c) = next_char(text, q) else {
                break;
            };
            if !c.is_whitespace() {
                break;
            }
            q += c.len_utf8();
            leading += 1;
        }
        let mut hashes = 0;
        while hashes < 6 && text[q..].starts_with('#') {
            q += 1;
            hashes += 1;
        }
        if hashes == 0 {
            return None;
        }
        Some(skip_whitespace(text, q))
    })
}

/// Unwraps `*italic*` and `**bold**` spans, allowing mixed delimiter lengths.
fn strip_star_emphasis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find('*') {
        out.push_str(&rest[..idx]);
        let after_first = &rest[idx + 1..];
        let open_len = if after_first.starts_with('*') { 2 } else { 1 };
        let body_rest = &rest[idx + open_len..];
        let body_len = body_rest.bytes().take_while(|&b| b != b'*').count();
        let after_body = &body_rest[body_len..];
        if body_len == 0 || !after_body.starts_with('*') {
            out.push('*');
            rest = after_first;
            continue;
        }
        let close_len = if after_body.as_bytes().get(1) == Some(&b'*') {
            2
        } else {
            1
        };
        out.push_str(&body_rest[..body_len]);
        rest = &after_body[close_len..];
    }
    out.push_str(rest);
    out
}

/// Unwraps spans delimited by the same fixed marker on both sides.
fn strip_delimited_pair(
    text: &str,
    delimiter: &str,
    body_exclude: char,
    allow_empty: bool,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(delimiter) {
        out.push_str(&rest[..idx]);
        let after_open = &rest[idx + delimiter.len()..];
        let body_len = after_open
            .chars()
            .take_while(|&c| c != body_exclude)
            .map(char::len_utf8)
            .sum::<usize>();
        let body = &after_open[..body_len];
        let after_body = &after_open[body_len..];
        if (allow_empty || body_len > 0) && after_body.starts_with(delimiter) {
            out.push_str(body);
            rest = &after_body[delimiter.len()..];
            continue;
        }
        if let Some(first) = delimiter.chars().next() {
            out.push(first);
            rest = &rest[idx + first.len_utf8()..];
        } else {
            out.push_str(rest);
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Removes bullet markers from list lines.
fn strip_bullets(text: &str) -> String {
    transform_line_prefixes(text, |text, line_start| {
        let q = skip_whitespace(text, line_start);
        let marker = next_char(text, q)?;
        if !matches!(marker, '-' | '*' | '+') {
            return None;
        }
        let mut end = q + marker.len_utf8();
        let mut count = 0;
        while let Some(c) = next_char(text, end) {
            if !c.is_whitespace() {
                break;
            }
            end += c.len_utf8();
            count += 1;
        }
        if count > 0 { Some(end) } else { None }
    })
}

/// Removes numbering markers from ordered list lines.
fn strip_numbering(text: &str) -> String {
    transform_line_prefixes(text, |text, line_start| {
        let q = skip_whitespace(text, line_start);
        let digits = text[q..].bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !text[q + digits..].starts_with('.') {
            return None;
        }
        let mut end = q + digits + 1;
        let mut count = 0;
        while let Some(c) = next_char(text, end) {
            if !c.is_whitespace() {
                break;
            }
            end += c.len_utf8();
            count += 1;
        }
        if count > 0 { Some(end) } else { None }
    })
}

/// Returns the length consumed by a `prefix[label](target)` span and its label.
fn bracket_span<'a>(rest: &'a str, prefix: &str, allow_empty: bool) -> Option<(usize, &'a str)> {
    let after_prefix = rest.strip_prefix(prefix)?;
    let label_end = after_prefix.find(']').filter(|&n| allow_empty || n > 0)?;
    let label = &after_prefix[..label_end];
    let target = after_prefix[label_end + 1..].strip_prefix('(')?;
    let target_end = target.find(')')?;
    let consumed = prefix.len() + label_end + 1 + 1 + target_end + 1;
    Some((consumed, label))
}

/// Replaces markdown links or images with the label or a single space.
fn substitute_bracketed(
    text: &str,
    prefix: &str,
    allow_empty: bool,
    replacement: impl Fn(&str) -> &str,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(prefix) {
        out.push_str(&rest[..idx]);
        let candidate = &rest[idx..];
        if let Some((consumed, label)) = bracket_span(candidate, prefix, allow_empty) {
            out.push_str(replacement(label));
            rest = &candidate[consumed..];
        } else if let Some(first) = prefix.chars().next() {
            out.push(first);
            rest = &candidate[first.len_utf8()..];
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Removes blockquote markers from line starts.
fn strip_quotes(text: &str) -> String {
    transform_line_prefixes(text, |text, line_start| {
        let q = skip_whitespace(text, line_start);
        let after = text[q..].strip_prefix('>')?;
        let mut end = q + 1;
        if let Some(c) = after.chars().next()
            && c.is_whitespace()
        {
            end += c.len_utf8();
        }
        Some(end)
    })
}

/// Removes horizontal rule lines.
fn strip_rules(text: &str) -> String {
    transform_line_prefixes(text, |text, line_start| {
        let mut p = skip_whitespace(text, line_start);
        let mut marks = 0;
        while let Some(c) = next_char(text, p) {
            if !matches!(c, '-' | '*' | '_') {
                break;
            }
            p += c.len_utf8();
            marks += 1;
        }
        if marks < 3 {
            return None;
        }
        let mut end = p;
        let mut before_last_newline = None;
        while let Some(c) = next_char(text, end) {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' {
                before_last_newline = Some(end);
            }
            end += c.len_utf8();
        }
        if end == text.len() {
            Some(end)
        } else {
            before_last_newline
        }
    })
}

/// Replaces bare web addresses with the spoken word "a link".
fn replace_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let starts = [
            rest.find("http://"),
            rest.find("https://"),
            rest.find("www."),
        ];
        let Some(start) = starts.into_iter().flatten().min() else {
            out.push_str(rest);
            break;
        };
        let tail = &rest[start..];
        let prefix_len = if tail.starts_with("https://") {
            8
        } else if tail.starts_with("http://") {
            7
        } else {
            4
        };
        let after_prefix = &tail[prefix_len..];
        let run_len = after_prefix
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | ']' | '>' | '"' | '\''))
            .map(char::len_utf8)
            .sum::<usize>();
        if run_len == 0 {
            out.push_str(&rest[..start + prefix_len]);
            rest = &tail[prefix_len..];
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str("a link");
        rest = &tail[prefix_len + run_len..];
    }
    out
}

/// Returns true for characters allowed in a path segment.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '+' | '-')
}

/// Returns true for characters that block the absolute-path match.
fn is_path_lookbehind_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '/' | '.' | '~')
}

/// Length of the maximal path-segment character run at an offset.
fn path_run_len(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .take_while(|&c| is_path_char(c))
        .map(char::len_utf8)
        .sum()
}

/// Length of the leading `[\w+-]+` part of the path lookahead.
fn path_lookahead_len(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .take_while(|&c| c.is_alphanumeric() || matches!(c, '_' | '+' | '-'))
        .map(char::len_utf8)
        .sum()
}

/// Matches a slash-separated path at `at`, returning its end and final segment.
fn match_path_at(text: &str, at: usize) -> Option<(usize, &str)> {
    let before = text[..at].chars().next_back();
    let here = next_char(text, at)?;

    if !before.is_some_and(is_path_lookbehind_char) {
        let mut pos = at;
        if text[pos..].starts_with('~') {
            pos += 1;
        } else if text[pos..].starts_with("..") {
            pos += 2;
        } else if text[pos..].starts_with('.') {
            pos += 1;
        }
        if text[pos..].starts_with('/') {
            pos += 1;
            let mut capture = None;
            loop {
                let len = path_run_len(text, pos);
                if len == 0 {
                    break;
                }
                capture = Some((pos, pos + len));
                if text[pos + len..].starts_with('/') {
                    pos += len + 1;
                } else {
                    break;
                }
            }
            if let Some((start, end)) = capture {
                return Some((end, &text[start..end]));
            }
        }
    }

    let before_word = before.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let here_word = here.is_alphanumeric() || here == '_';
    if before_word != here_word {
        let mut pos = at;
        let mut candidates = Vec::new();
        loop {
            let len = path_run_len(text, pos);
            if len == 0 || !text[pos + len..].starts_with('/') {
                break;
            }
            pos += len + 1;
            candidates.push(pos);
        }
        for candidate in candidates.into_iter().rev() {
            let look = path_lookahead_len(text, candidate);
            let has_dot = text[candidate + look..].starts_with('.');
            let followed_by_word = text
                .get(candidate + look + 1..)
                .and_then(|tail| tail.chars().next())
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if look > 0 && has_dot && followed_by_word {
                let len = path_run_len(text, candidate);
                return Some((candidate + len, &text[candidate..candidate + len]));
            }
        }
    }
    None
}

/// Speaks only the final component of file paths, leaving slash-in-words alone.
fn shorten_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((end, label)) = match_path_at(text, i) {
            out.push_str(label);
            i = end;
        } else if let Some(c) = next_char(text, i) {
            out.push(c);
            i += c.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// Removes emoji, variation selectors and joiners from the utterance.
fn strip_emoji(text: &str) -> String {
    text.chars()
        .filter(|c| {
            let cp = *c as u32;
            !(0x1F000..=0x1FAFF).contains(&cp)
                && !(0x2600..=0x27BF).contains(&cp)
                && cp != 0xFE0F
                && cp != 0x200D
        })
        .collect()
}

/// Collapses runs of spaces and tabs without touching line breaks.
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c == ' ' || c == '\t' {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// Turns blank lines into sentence breaks or soft pauses.
fn join_paragraphs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(c) = next_char(text, i) else {
            break;
        };
        if c != '\n' {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let mut j = i + 1;
        let mut last_newline_end = None;
        while let Some(w) = next_char(text, j) {
            if !w.is_whitespace() {
                break;
            }
            j += w.len_utf8();
            if w == '\n' {
                last_newline_end = Some(j);
            }
        }
        if let Some(end) = last_newline_end {
            // Python's `"" in ".!?:"` is true, so a leading blank line is a
            // soft pause rather than an inserted sentence break.
            let before = text[..start].trim_end();
            if before
                .chars()
                .next_back()
                .is_none_or(|last| matches!(last, '.' | '!' | '?' | ':'))
            {
                out.push(' ');
            } else {
                out.push_str(". ");
            }
            i = end;
        } else {
            out.push('\n');
            i = start + 1;
        }
    }
    out
}

/// Replaces soft line breaks (and leftover newlines) with a single space.
fn join_soft_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let Some(c) = next_char(text, i) else {
            break;
        };
        if !c.is_whitespace() {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let mut has_newline = false;
        while let Some(w) = next_char(text, i) {
            if !w.is_whitespace() {
                break;
            }
            if w == '\n' {
                has_newline = true;
            }
            i += w.len_utf8();
        }
        if has_newline {
            out.push(' ');
        } else {
            out.push_str(&text[start..i]);
        }
    }
    out
}

/// True when the dot at `at` belongs to a number such as `1.2.3`.
fn is_decimal_dot(text: &str, at: usize) -> bool {
    let bytes = text.as_bytes();
    if at == 0 || !bytes[at - 1].is_ascii_digit() {
        return false;
    }
    let mut i = at - 1;
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'.' {
        return false;
    }
    i -= 1;
    i > 0 && bytes[i - 1].is_ascii_digit()
}

/// Returns byte spans of punctuation (plus closing quotes) that end sentences.
fn sentence_end_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(c) = next_char(text, i) {
        if !matches!(c, '.' | '!' | '?') || (c == '.' && is_decimal_dot(text, i)) {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        let mut j = i;
        while let Some(ch) = next_char(text, j) {
            if !matches!(ch, '.' | '!' | '?') {
                break;
            }
            j += ch.len_utf8();
        }
        while let Some(ch) = next_char(text, j) {
            if !matches!(ch, '"' | '\'' | ')' | ']') {
                break;
            }
            j += ch.len_utf8();
        }
        if next_char(text, j).is_none_or(char::is_whitespace) {
            spans.push((start, j));
            i = j;
        } else {
            i += c.len_utf8();
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_fenced_code() {
        assert_eq!(
            sanitize_for_tts("Here:\n```python\nprint(1)\n```\ndone."),
            "Here: done."
        );
        assert_eq!(
            sanitize_for_tts("Run this:\n```sh\nrm -rf /\n```\nthen stop."),
            "Run this: then stop."
        );
        assert_eq!(
            sanitize_for_tts("Unterminated ```code here"),
            "Unterminated"
        );
    }

    #[test]
    fn sanitize_keeps_inline_code_as_prose() {
        assert_eq!(
            sanitize_for_tts("Run `cargo test` now."),
            "Run cargo test now."
        );
        assert_eq!(
            sanitize_for_tts(
                "Edited `apps/navi/src/main.rs` and /home/amr/Documents/navi/README.md today."
            ),
            "Edited main.rs and README.md today."
        );
    }

    #[test]
    fn sanitize_strips_headings_and_emphasis() {
        assert_eq!(
            sanitize_for_tts("## Result\n\n**bold** answer"),
            "Result. bold answer"
        );
        assert_eq!(
            sanitize_for_tts("***bold*** and __strong__ and ~~gone~~"),
            "*bold* and strong and gone"
        );
    }

    #[test]
    fn sanitize_collapses_links_and_urls() {
        assert_eq!(
            sanitize_for_tts("see [the docs](https://x.test) here"),
            "see the docs here"
        );
        assert_eq!(
            sanitize_for_tts("Done. See https://example.com/a/b?x=1 for details."),
            "Done. See a link for details."
        );
        assert_eq!(
            sanitize_for_tts("www.example.com/path?x=1 done"),
            "a link done"
        );
        assert_eq!(
            sanitize_for_tts("A [link](x) and ![img](y) here"),
            "A link and !img here"
        );
        assert_eq!(
            sanitize_for_tts("http:// and http://) stay"),
            "http:// and http://) stay"
        );
    }

    #[test]
    fn sanitize_shortens_paths_without_touching_slash_words() {
        assert_eq!(
            sanitize_for_tts("Use and/or TCP/IP on 1/2."),
            "Use and/or TCP/IP on 1/2."
        );
        assert_eq!(
            sanitize_for_tts("src/main.rs and ~/docs/notes.txt"),
            "main.rs and notes.txt"
        );
    }

    #[test]
    fn sanitize_strips_bullets_numbers_rules_and_tables() {
        let out = sanitize_for_tts("- one\n- two\n1. first\n2. second");
        assert_eq!(out, "one two first second");
        assert!(!out.contains('-'));
        assert!(!out.contains("1."));
        assert_eq!(
            sanitize_for_tts("> quoted line\n---\n| a | b |"),
            "quoted line. , a , b ,"
        );
        assert_eq!(
            sanitize_for_tts("a | b\n---\n> quote\n\n1. x\n2. y"),
            "a , b. quote x y"
        );
    }

    #[test]
    fn sanitize_drops_bookkeeping_lines_and_emoji() {
        let text = "Fixed the bug.\n\nPROGRESS part \"Voice\" 80 | tests pass\nPROGRESS overall 70 | read the code";
        assert_eq!(sanitize_for_tts(text), "Fixed the bug.");
        assert_eq!(
            sanitize_for_tts("Great news! \u{1F600} All tests pass."),
            "Great news! All tests pass."
        );
        assert_eq!(sanitize_for_tts("  PROGRESS overall 50 | checked"), "");
    }

    #[test]
    fn split_sentences_ignores_short_forms_and_decimals() {
        let text = "Version 1.2.3 is out, e.g. on npm. It works.";
        assert_eq!(
            split_sentences(text),
            ["Version 1.2.3 is out, e.g. on npm.", "It works."]
        );
        assert_eq!(
            split_sentences("One. Two! Three? Four. Five."),
            ["One.", "Two!", "Three?", "Four.", "Five."]
        );
        assert!(split_sentences("").is_empty());
    }

    #[test]
    fn first_sentences_caps_to_requested_count() {
        assert_eq!(
            first_sentences("All systems nominal.", 3),
            "All systems nominal."
        );
        assert_eq!(
            first_sentences("One. Two! Three? Four. Five.", 3),
            "One. Two! Three?"
        );
        assert_eq!(
            first_sentences("One. Two! Three? Four. Five.", 2),
            "One. Two!"
        );
        assert_eq!(first_sentences("", 3), "");
    }

    #[test]
    fn first_sentences_cuts_long_text_at_word_boundary() {
        let spoken = first_sentences(&"word ".repeat(300), 3);
        assert_eq!(spoken.chars().count(), 600);
        assert!(spoken.ends_with('.'));
        assert!(!spoken.contains("  "));
        assert!(spoken.starts_with("word word"));
    }

    #[test]
    fn first_sentences_ignores_spoken_markdown() {
        let spoken = first_sentences("Run this:\n```sh\nrm -rf /\n```\nthen stop.", 3);
        assert_eq!(spoken, "Run this: then stop.");
        assert!(!spoken.contains("rm -rf"));
        assert!(spoken.contains("then stop."));
    }

    #[test]
    fn plan_carries_metadata_and_truncation_flag() {
        let plan = plan_speech_output("One. Two! Three?", "navi", "en-GB-RyanNeural", "+0%", 3);
        assert_eq!(plan.persona, "navi");
        assert_eq!(plan.voice, "en-GB-RyanNeural");
        assert_eq!(plan.rate, "+0%");
        assert_eq!(plan.text, "One. Two! Three?");
        assert_eq!(plan.spoken_text, "One. Two! Three?");
        assert!(!plan.truncated);

        let short = plan_speech_output("One. Two! Three?", "navi", "voice", "+0%", 1);
        assert_eq!(short.spoken_text, "One.");
        assert!(short.truncated);
        assert_eq!(short.text, "One. Two! Three?");

        let long = plan_speech_output(&"word ".repeat(300), "navi", "voice", "+0%", 3);
        assert!(long.truncated);
        assert_eq!(long.spoken_text.chars().count(), 600);
    }

    #[test]
    fn plan_handles_empty_and_whitespace_only_replies() {
        let empty = plan_speech_output("", "navi", "voice", "+0%", 3);
        assert_eq!(empty.text, "");
        assert_eq!(empty.spoken_text, "");
        assert!(!empty.truncated);

        let blank = plan_speech_output("  \n\t ", "navi", "voice", "+0%", 3);
        assert_eq!(blank.text, "");
        assert_eq!(blank.spoken_text, "");
        assert!(!blank.truncated);
        assert_eq!(blank.persona, "navi");
        assert_eq!(blank.voice, "voice");
        assert_eq!(blank.rate, "+0%");
    }

    #[test]
    fn sanitize_is_idempotent_and_handles_empty() {
        let raw = "## Result\n\n**bold** [link](https://x.test)\n\n- item\n\nDone.";
        let once = sanitize_for_tts(raw);
        assert_eq!(sanitize_for_tts(&once), once);
        assert_eq!(sanitize_for_tts(""), "");
        assert_eq!(first_sentences("   \n\n  ", 3), "");
    }
}
