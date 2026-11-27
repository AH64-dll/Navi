//! Email header, MIME, address and summary parsing for the Navi mail connector.
//!
//! Ported from `apps/navi/workers/mail.py`. All inputs here are raw messages
//! fetched with `BODY.PEEK`, so parsing has no side effects on message flags.

use serde::{Deserialize, Serialize};

/// One mailbox: a display name and the addr-spec without angle brackets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAddress {
    pub name: String,
    pub address: String,
}

/// Compact per-message view used by the check and read operations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailSummary {
    pub uid: String,
    pub from: MailAddress,
    pub to: MailAddress,
    pub subject: String,
    pub date: String,
    pub snippet: String,
    pub unread: bool,
}

impl MailAddress {
    /// Returns the display name, then the address, then a spoken fallback.
    pub fn display_name(&self) -> &str {
        if !self.name.is_empty() {
            &self.name
        } else if !self.address.is_empty() {
            &self.address
        } else {
            "someone unknown"
        }
    }
}

/// Decodes RFC 2047 `=?charset?Q|B?text?=` words in a header value.
///
/// Linear whitespace between two adjacent encoded words is dropped, as the
/// RFC requires; malformed or unsupported words are left untouched.
pub fn decode_rfc2047(raw: &str) -> String {
    enum Segment {
        Text(String),
        Word(String),
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut cursor = 0usize;
    while let Some(start_rel) = raw[cursor..].find("=?") {
        let start = cursor + start_rel;
        let Some((decoded, length)) = decode_word_at(&raw[start..]) else {
            break;
        };
        if start > cursor {
            segments.push(Segment::Text(raw[cursor..start].to_string()));
        }
        segments.push(Segment::Word(decoded));
        cursor = start + length;
    }
    if cursor < raw.len() {
        segments.push(Segment::Text(raw[cursor..].to_string()));
    }

    let mut out = String::with_capacity(raw.len());
    let count = segments.len();
    for (index, segment) in segments.iter().enumerate() {
        match segment {
            Segment::Word(text) => out.push_str(text),
            Segment::Text(text) => {
                let between_words = index > 0
                    && index + 1 < count
                    && matches!(&segments[index - 1], Segment::Word(_))
                    && matches!(&segments[index + 1], Segment::Word(_));
                if between_words && text.chars().all(|ch| ch.is_ascii_whitespace()) {
                    continue;
                }
                out.push_str(text);
            }
        }
    }
    out
}

/// Decodes one encoded word at the start of `text`, returning the decoded
/// text and the number of bytes the word occupied. `None` means malformed.
///
/// The terminator is located after the encoding field, not by searching for
/// the first `?=` in the whole word: a Q payload may begin with `=XX`, which
/// would turn the `?Q?=` delimiter into a false terminator.
fn decode_word_at(text: &str) -> Option<(String, usize)> {
    let rest = text.strip_prefix("=?")?;
    let (charset, rest) = rest.split_once('?')?;
    let (encoding, rest) = rest.split_once('?')?;
    let (payload, after_word) = rest.split_once("?=")?;
    if charset.is_empty() || encoding.is_empty() || payload.contains('?') {
        return None;
    }
    let bytes = match encoding {
        "Q" | "q" => decode_q_word(payload)?,
        "B" | "b" => decode_base64(payload)?,
        _ => return None,
    };
    let length = text.len() - after_word.len();
    Some((bytes_to_text(charset, &bytes), length))
}

/// RFC 2047 Q encoding: `_` is a space and `=XX` is one byte.
fn decode_q_word(payload: &str) -> Option<Vec<u8>> {
    let bytes = payload.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'_' => {
                out.push(b' ');
                index += 1;
            }
            b'=' => {
                if index + 2 < bytes.len() && bytes[index + 1] == b'\r' && bytes[index + 2] == b'\n'
                {
                    index += 3;
                    continue;
                }
                let high = hex_value(*bytes.get(index + 1)?)?;
                let low = hex_value(*bytes.get(index + 2)?)?;
                out.push((high << 4) | low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    Some(out)
}

/// Decodes standard base64, ignoring whitespace and padding bytes.
fn decode_base64(payload: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(payload.len() * 3 / 4);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    let mut seen_padding = false;
    for byte in payload.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            seen_padding = true;
            continue;
        }
        if seen_padding {
            return None;
        }
        accumulator = (accumulator << 6) | u32::from(base64_value(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
            accumulator &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Bytes 0x80..0x9f in Windows-1252; undefined slots decode as replacement.
const CP1252_HIGH: [char; 32] = [
    '\u{20ac}', '\u{fffd}', '\u{201a}', '\u{0192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02c6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{fffd}', '\u{017d}', '\u{fffd}',
    '\u{fffd}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02dc}', '\u{2122}', '\u{0161}', '\u{203a}', '\u{0153}', '\u{fffd}', '\u{017e}', '\u{0178}',
];

fn bytes_to_text(charset: &str, bytes: &[u8]) -> String {
    let name = charset
        .split('*')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match name.as_str() {
        "iso-8859-1" | "iso8859-1" | "latin-1" | "latin1" | "l1" => {
            bytes.iter().copied().map(char::from).collect()
        }
        "windows-1252" | "cp1252" => bytes
            .iter()
            .map(|byte| {
                if (0x80..=0x9f).contains(byte) {
                    CP1252_HIGH[usize::from(*byte - 0x80)]
                } else {
                    char::from(*byte)
                }
            })
            .collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Parses the first mailbox of a header, decoding encoded words in its name.
pub fn parse_address(raw: &str) -> MailAddress {
    let decoded = decode_rfc2047(raw);
    let Some(first) = split_outside_quotes(&decoded, ',').first().copied() else {
        return MailAddress::default();
    };
    parse_mailbox(first.trim())
}

fn parse_mailbox(text: &str) -> MailAddress {
    if let Some(open) = text.find('<')
        && let Some(close_rel) = text[open + 1..].find('>')
    {
        let close = open + 1 + close_rel;
        let address = text[open + 1..close].trim().to_string();
        let before = text[..open].trim();
        let after = text[close + 1..].trim();
        let name_source = if before.is_empty() { after } else { before };
        return MailAddress {
            name: clean_display_name(name_source),
            address,
        };
    }
    let bare = strip_comments(text).trim().to_string();
    // A bare addr-spec must not carry an unmatched bracket: that means the
    // angle form was malformed, not that the mailbox is a bare address.
    let plausible = bare.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    });
    if plausible && !bare.contains(['<', '>', '[', ']']) {
        MailAddress {
            name: trailing_comment_name(text).unwrap_or_default(),
            address: bare,
        }
    } else {
        MailAddress::default()
    }
}

fn clean_display_name(source: &str) -> String {
    let stripped = strip_comments(source);
    let source = stripped.trim();
    let unquoted = if source.len() >= 2 && source.starts_with('"') && source.ends_with('"') {
        &source[1..source.len() - 1]
    } else {
        source
    };
    unquoted.replace("\\\"", "\"").trim().to_string()
}

/// Removes parenthesised comments, leaving quoted characters intact.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            if depth == 0 {
                out.push(ch);
            }
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            if depth == 0 {
                out.push(ch);
            }
            continue;
        }
        if in_quote {
            out.push(ch);
            if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_quote = true;
                out.push(ch);
            }
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 {
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// A trailing `(Name)` comment is the display name of a bare addr-spec.
fn trailing_comment_name(text: &str) -> Option<String> {
    let text = text.trim();
    if !text.ends_with(')') {
        return None;
    }
    let mut in_quote = false;
    let mut escaped = false;
    let mut open = None;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quote {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quote = !in_quote;
            continue;
        }
        if !in_quote && ch == '(' {
            open = Some(index);
        }
    }
    open.map(|index| clean_display_name(&text[index + 1..text.len() - 1]))
}

/// Splits on a delimiter that is outside quotes and bracketed groups.
fn split_outside_quotes(text: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut in_quote = false;
    let mut escaped = false;
    let mut depth = 0i32;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quote {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            continue;
        }
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth -= 1,
            found if found == delimiter && depth <= 0 => {
                parts.push(&text[start..index]);
                start = index + found.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Returns the display name of a From header as `sender_name` does in Python.
pub fn sender_name(raw: &str) -> String {
    parse_address(raw).display_name().to_string()
}

/// Removes HTML markup, skipping script/style/head content and decoding entities.
pub fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut skipping = 0usize;
    let mut rest = html;
    while let Some(rel) = rest.find('<') {
        if skipping == 0 {
            out.push_str(&decode_html_entities(&rest[..rel]));
        }
        rest = &rest[rel + 1..];
        if rest.starts_with("!--") {
            match rest[3..].find("-->") {
                Some(end) => rest = &rest[3 + end + 3..],
                None => rest = "",
            }
            continue;
        }
        let Some(end) = rest.find('>') else {
            break;
        };
        let markup = &rest[..end];
        rest = &rest[end + 1..];
        let trimmed = markup.trim();
        let closing = trimmed.starts_with('/');
        let inner = if closing {
            trimmed[1..].trim_start()
        } else {
            trimmed
        };
        let name: String = inner
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let self_closing = inner.trim_end().ends_with('/');
        match name.as_str() {
            "script" | "style" | "head" => {
                if closing {
                    skipping = skipping.saturating_sub(1);
                } else if !self_closing {
                    skipping += 1;
                }
            }
            "p" | "div" | "tr" | "li" => {
                if closing {
                    out.push('\n');
                }
            }
            "br" => out.push('\n'),
            _ => {}
        }
    }
    if skipping == 0 {
        out.push_str(&decode_html_entities(rest));
    }
    out
}

fn decode_html_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(rel) = rest.find('&') {
        out.push_str(&rest[..rel]);
        rest = &rest[rel + 1..];
        let Some(end) = rest.find(';') else {
            out.push('&');
            out.push_str(rest);
            return out;
        };
        let entity = &rest[..end];
        let decoded = if let Some(hex) = entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(decimal) = entity.strip_prefix('#') {
            decimal.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => None,
            }
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Extracts readable text from a raw message, preferring plain over HTML.
pub fn body_text(raw: &str) -> String {
    let root = parse_mime_part(raw);
    let selected = select_body_part(&root, "plain").or_else(|| select_body_part(&root, "html"));
    let Some(selected) = selected else {
        return String::new();
    };
    let content = if selected.content_type == "text/html" {
        strip_html_tags(&selected.body)
    } else {
        selected.body.clone()
    };
    tidy_text(&content)
}

#[derive(Default)]
struct MimePart {
    content_type: String,
    transfer_encoding: String,
    body: String,
    children: Vec<MimePart>,
}

fn parse_mime_part(raw: &str) -> MimePart {
    let (header_block, body) = split_headers_body(raw);
    let headers = unfold_headers(header_block);
    let content_type = header_value(&headers, "content-type");
    let (mime_type, params) = parse_content_type(&content_type);
    let mime_type = if mime_type.is_empty() {
        "text/plain".to_string()
    } else {
        mime_type
    };
    let mut part = MimePart {
        content_type: mime_type,
        transfer_encoding: header_value(&headers, "content-transfer-encoding"),
        body: String::new(),
        children: Vec::new(),
    };
    if part.content_type.starts_with("multipart/")
        && let Some(boundary) = params
            .iter()
            .find_map(|(name, value)| (name == "boundary").then_some(value.as_str()))
    {
        for child in split_multipart(body, boundary) {
            part.children.push(parse_mime_part(&child));
        }
        return part;
    }
    part.body = decode_transfer(body, &part.transfer_encoding);
    part
}

fn parse_content_type(value: &str) -> (String, Vec<(String, String)>) {
    let (mime, rest) = match value.split_once(';') {
        Some((mime, rest)) => (mime.trim().to_ascii_lowercase(), rest),
        None => (value.trim().to_ascii_lowercase(), ""),
    };
    let mut params = Vec::new();
    for chunk in split_outside_quotes(rest, ';') {
        if let Some((name, value)) = chunk.trim().split_once('=') {
            params.push((
                name.trim().to_ascii_lowercase(),
                value.trim().trim_matches('"').to_string(),
            ));
        }
    }
    (mime, params)
}

fn split_multipart(body: &str, boundary: &str) -> Vec<String> {
    let marker = format!("--{boundary}");
    let closing = format!("{marker}--");
    let mut parts = Vec::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        if line == marker || line == closing {
            if let Some(part) = current.take() {
                parts.push(part);
            }
            current = Some(String::new());
        } else if let Some(part) = current.as_mut() {
            part.push_str(line);
            part.push('\n');
        }
    }
    if let Some(part) = current {
        parts.push(part);
    }
    parts
}

fn decode_transfer(body: &str, encoding: &str) -> String {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "base64" => {
            let mut bytes = Vec::with_capacity(body.len() * 3 / 4);
            for line in body.lines() {
                if let Some(decoded) = decode_base64(line.trim()) {
                    bytes.extend_from_slice(&decoded);
                }
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
        "quoted-printable" => decode_quoted_printable(body),
        _ => body.to_string(),
    }
}

fn decode_quoted_printable(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'=' {
            if index + 2 < bytes.len() && bytes[index + 1] == b'\r' && bytes[index + 2] == b'\n' {
                index += 3;
                continue;
            }
            if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                index += 2;
                continue;
            }
            if index + 2 < bytes.len()
                && let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn select_body_part<'a>(part: &'a MimePart, subtype: &str) -> Option<&'a MimePart> {
    if part.children.is_empty() {
        return (part.content_type.strip_prefix("text/") == Some(subtype)).then_some(part);
    }
    for child in &part.children {
        if let Some(found) = select_body_part(child, subtype) {
            return Some(found);
        }
    }
    None
}

fn tidy_text(content: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if line.starts_with('>') || trimmed == "--" || is_on_wrote_line(trimmed) {
            break;
        }
        lines.push(line.trim_end());
    }
    collapse_newlines(&lines.join("\n")).trim().to_string()
}

fn is_on_wrote_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("On ") else {
        return false;
    };
    let Some(prefix) = rest.strip_suffix("wrote:") else {
        return false;
    };
    !prefix.trim().is_empty() && prefix.chars().last().is_some_and(|ch| ch.is_whitespace())
}

fn collapse_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            run += 1;
            if run <= 2 {
                out.push(ch);
            }
        } else {
            run = 0;
            out.push(ch);
        }
    }
    out
}

/// Number of characters kept in a snippet before it is cut on a word boundary.
pub const DEFAULT_SNIPPET_CHARS: usize = 3000;

/// Truncates a body to `max_chars` characters, ending on a word boundary.
pub fn body_preview(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }
    let mut out: String = body.chars().take(max_chars).collect();
    if let Some(index) = out.rfind(' ') {
        out.truncate(index);
    }
    out.push(' ');
    out.push('\u{2026}');
    out
}

/// Builds a compact summary from one raw RFC 5322 message.
pub fn parse_raw_email(uid: &str, raw: &str) -> MailSummary {
    let (header_block, _) = split_headers_body(raw);
    let headers = unfold_headers(header_block);
    let subject = header_value(&headers, "subject");
    MailSummary {
        uid: uid.to_string(),
        from: parse_address(&header_value(&headers, "from")),
        to: parse_address(&header_value(&headers, "to")),
        subject: if subject.is_empty() {
            "no subject".to_string()
        } else {
            subject
        },
        date: header_value(&headers, "date"),
        snippet: body_preview(&body_text(raw), DEFAULT_SNIPPET_CHARS),
        unread: is_unread(&headers),
    }
}

fn is_unread(headers: &[(String, String)]) -> bool {
    for (name, value) in headers {
        match name.as_str() {
            "x-mozilla-status" => {
                if let Ok(flags) = u32::from_str_radix(value.trim(), 16) {
                    return flags & 0x0001 == 0;
                }
            }
            "status" | "x-status" if value.contains('R') => return false,
            _ => {}
        }
    }
    true
}

/// Formats one `- {name}: {subject}` line of the check listing.
pub fn format_check_row(summary: &MailSummary) -> String {
    let who = summary.from.display_name();
    let subject = &summary.subject;
    format!("- {who}: {subject}")
}

/// Formats the user-visible text of the check operation.
pub fn format_check_text(unread_count: usize, rows: &[MailSummary]) -> String {
    let plural = if unread_count == 1 { "email" } else { "emails" };
    let mut text = format!("You have {unread_count} unread {plural}.");
    for row in rows {
        text.push('\n');
        text.push_str(&format_check_row(row));
    }
    text
}

/// Formats the spoken check summary, naming the newest message when present.
pub fn format_check_speech(unread_count: usize, newest: Option<&MailSummary>) -> String {
    let plural = if unread_count == 1 { "email" } else { "emails" };
    let mut speech = format!("You have {unread_count} unread {plural}.");
    if let Some(summary) = newest {
        speech.push_str(&format!(
            " The newest is from {}, about {}.",
            summary.from.display_name(),
            summary.subject
        ));
    }
    speech
}

/// Formats the user-visible text of the read operation.
pub fn format_read_text(summary: &MailSummary) -> String {
    format!(
        "From {}: {}\n\n{}",
        summary.from.display_name(),
        summary.subject,
        summary.snippet
    )
    .trim_end()
    .to_string()
}

/// Formats the spoken read summary; `said` is the spoken body sentence.
pub fn format_read_speech(summary: &MailSummary, said: &str) -> String {
    format!(
        "From {}, about {}. {said}",
        summary.from.display_name(),
        summary.subject
    )
}

fn split_headers_body(raw: &str) -> (&str, &str) {
    let mut line_start = 0usize;
    while line_start < raw.len() {
        let Some(rel) = raw[line_start..].find('\n') else {
            break;
        };
        let line_end = line_start + rel;
        let line = &raw[line_start..line_end];
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            return (&raw[..line_start], &raw[line_end + 1..]);
        }
        line_start = line_end + 1;
    }
    (raw, "")
}

fn unfold_headers(block: &str) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in block.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    headers
}

fn header_value(headers: &[(String, String)], name: &str) -> String {
    for (key, value) in headers {
        if key == name {
            return decode_rfc2047(value).trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_handles_common_forms() {
        let alice = parse_address("Alice Example <alice@example.com>");
        assert_eq!(alice.name, "Alice Example");
        assert_eq!(alice.address, "alice@example.com");

        let bare = parse_address("bob@example.org");
        assert_eq!(bare.name, "");
        assert_eq!(bare.address, "bob@example.org");
        assert_eq!(parse_address("<carol@example.net>").name, "");

        let quoted = parse_address("\"Doe, Alice\" <doe@example.com>");
        assert_eq!(quoted.name, "Doe, Alice");
        assert_eq!(quoted.address, "doe@example.com");

        let first = parse_address("Bob <bob@example.com>, Carol <carol@example.com>");
        assert_eq!(first.address, "bob@example.com");
    }

    #[test]
    fn parse_address_rejects_unmatched_brackets_and_non_addresses() {
        assert_eq!(parse_address("<broken@example.com"), MailAddress::default());
        assert_eq!(parse_address("broken@example.com>"), MailAddress::default());
        assert_eq!(parse_address("not an address"), MailAddress::default());
        assert_eq!(parse_address("@example.com"), MailAddress::default());
    }

    #[test]
    fn parse_address_decodes_words_and_reads_comments() {
        let andre = parse_address("=?UTF-8?Q?Andr=C3=A9?= <andre@example.com> (work)");
        assert_eq!(andre.name, "André");
        assert_eq!(andre.address, "andre@example.com");

        let chinese = parse_address("=?utf-8?B?5L2g5aW9?= <hi@example.cn>");
        assert_eq!(chinese.name, "你好");

        let comment = parse_address("dave@example.com (Dave)");
        assert_eq!(comment.name, "Dave");
        assert_eq!(comment.address, "dave@example.com");
        assert_eq!(sender_name(""), "someone unknown");
        assert_eq!(sender_name("ernie@example.com"), "ernie@example.com");
    }

    #[test]
    fn decode_rfc2047_handles_q_and_b_words() {
        assert_eq!(decode_rfc2047("=?UTF-8?Q?Hello=20World?="), "Hello World");
        assert_eq!(decode_rfc2047("=?UTF-8?Q?Hello_World?="), "Hello World");
        assert_eq!(decode_rfc2047("=?ISO-8859-1?Q?Andr=E9?="), "André");
        assert_eq!(decode_rfc2047("=?UTF-8?B?QW5kcsOp?="), "André");
        assert_eq!(decode_rfc2047("plain text"), "plain text");
        assert_eq!(decode_rfc2047(""), "");
    }

    #[test]
    fn decode_rfc2047_merges_adjacent_words_and_keeps_malformed() {
        assert_eq!(
            decode_rfc2047("=?UTF-8?Q?Hi?= =?UTF-8?Q?there?="),
            "Hithere"
        );
        assert_eq!(
            decode_rfc2047("plain =?UTF-8?Q?=E4=BD=A0=E5=A5=BD?= text"),
            "plain 你好 text"
        );
        assert_eq!(decode_rfc2047("=?UTF-8?Q?bad?"), "=?UTF-8?Q?bad?");
        assert_eq!(decode_rfc2047("=?UTF-8?X?nope?="), "=?UTF-8?X?nope?=");
        assert_eq!(
            decode_rfc2047("=?windows-1252?Q?=93Hi=94?="),
            "\u{201c}Hi\u{201d}"
        );
        assert_eq!(
            decode_rfc2047("=?UTF-8?B?not-base64!?="),
            "=?UTF-8?B?not-base64!?="
        );
    }

    #[test]
    fn parse_raw_email_builds_summary() {
        let raw = concat!(
            "From: Alice <alice@example.com>\r\n",
            "To: Bob <bob@example.com>\r\n",
            "Subject: =?UTF-8?Q?Caf=C3=A9_time?=\r\n",
            "Date: Wed, 21 Oct 2015 07:28:00 +0000\r\n",
            "\r\n",
            "Hello Bob,\r\n",
            "This is the first line.\r\n",
        );
        let summary = parse_raw_email("42", raw);
        assert_eq!(summary.uid, "42");
        assert_eq!(summary.from.address, "alice@example.com");
        assert_eq!(summary.to.address, "bob@example.com");
        assert_eq!(summary.subject, "Café time");
        assert_eq!(summary.date, "Wed, 21 Oct 2015 07:28:00 +0000");
        assert_eq!(summary.snippet, "Hello Bob,\nThis is the first line.");
        assert!(summary.unread);
    }

    #[test]
    fn parse_raw_email_prefers_plain_and_decodes_quoted_printable() {
        let raw = concat!(
            "From: Team <team@example.com>\r\n",
            "Subject: Report\r\n",
            "X-Mozilla-Status: 0008\r\n",
            "Content-Type: multipart/alternative; boundary=\"B1\"\r\n",
            "\r\n",
            "--B1\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<html><body><p>HTML version</p><script>secret()</script></body></html>\r\n",
            "--B1\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            "\r\n",
            "Caf=C3=A9 report=2E\r\n",
            "Thanks,\r\n",
            "-- \r\n",
            "Signature\r\n",
            "--B1--\r\n",
        );
        let summary = parse_raw_email("7", raw);
        assert_eq!(summary.subject, "Report");
        assert!(summary.unread);
        assert_eq!(summary.snippet, "Café report.\nThanks,");
    }

    #[test]
    fn parse_raw_email_unfolds_headers_and_handles_missing_fields() {
        let raw = concat!(
            "From: =?UTF-8?Q?Zo=C3=AB?= <zoe@example.org>\r\n",
            "Subject: a very long\r\n",
            "\tsubject line\r\n",
            "X-Status: R\r\n",
            "\r\n",
            "Body.\r\n",
        );
        let summary = parse_raw_email("9", raw);
        assert_eq!(summary.uid, "9");
        assert_eq!(summary.from.name, "Zoë");
        assert_eq!(summary.from.address, "zoe@example.org");
        assert_eq!(summary.to, MailAddress::default());
        assert_eq!(summary.subject, "a very long subject line");
        assert_eq!(summary.date, "");
        assert_eq!(summary.snippet, "Body.");
        assert!(!summary.unread);
    }

    #[test]
    fn strip_html_tags_skips_script_and_decodes_entities() {
        let html = concat!(
            "<html><head><title>hidden</title></head><body>",
            "<p>Hello &amp; welcome</p><script>alert(1)</script><br>A</body></html>",
        );
        let text = strip_html_tags(html);
        assert!(text.contains("Hello & welcome"));
        assert!(!text.contains("hidden"));
        assert!(!text.contains("alert"));
        assert!(text.contains("\nA"));
        assert_eq!(strip_html_tags("a &lt; b &#x26; c"), "a < b & c");
    }

    #[test]
    fn body_text_converts_html_and_stops_at_quoted_history() {
        let raw = concat!(
            "Content-Type: text/html; charset=utf-8\n",
            "\n",
            "<div>First &lt;line&gt;</div><div>On Tue Bob wrote:</div><div>quoted</div>\n",
        );
        assert_eq!(body_text(raw), "First <line>");
        assert_eq!(body_text("Subject: nothing\n\n"), "");
    }

    #[test]
    fn formatting_matches_the_python_summaries() {
        let summary = MailSummary {
            uid: "1".to_string(),
            from: parse_address("Alice <alice@example.com>"),
            to: parse_address("Bob <bob@example.com>"),
            subject: "Hi".to_string(),
            date: String::new(),
            snippet: "Hello there".to_string(),
            unread: true,
        };
        assert_eq!(format_check_row(&summary), "- Alice: Hi");
        assert_eq!(
            format_check_text(2, std::slice::from_ref(&summary)),
            "You have 2 unread emails.\n- Alice: Hi"
        );
        assert_eq!(
            format_check_speech(1, Some(&summary)),
            "You have 1 unread email. The newest is from Alice, about Hi."
        );
        assert_eq!(format_read_text(&summary), "From Alice: Hi\n\nHello there");
        assert_eq!(
            format_read_speech(&summary, "It has no text."),
            "From Alice, about Hi. It has no text."
        );
    }

    #[test]
    fn body_preview_truncates_on_word_boundary() {
        assert_eq!(body_preview("one two three", 100), "one two three");
        assert_eq!(body_preview("one two three", 8), "one two …");
        assert_eq!(body_preview("abcdef", 3), "abc …");
    }

    #[test]
    fn parse_raw_email_decodes_base64_body() {
        let raw = concat!(
            "From: Ada <ada@example.com>\r\n",
            "Subject: Encoded\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "SGVsbG8=\r\n",
        );
        let summary = parse_raw_email("b64", raw);
        assert_eq!(summary.from.address, "ada@example.com");
        assert_eq!(summary.snippet, "Hello");
    }
}
