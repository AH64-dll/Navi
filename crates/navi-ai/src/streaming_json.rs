//! Tolerant parsing and repair of JSON text that arrives in pieces.
//!
//! Streaming providers emit tool arguments incrementally, so a reader can see a
//! string cut in the middle of an escape, an object without its closing brace,
//! or a trailing comma. The functions here recover the value read so far and
//! turn damaged text back into syntactically valid JSON. They never fail on
//! untrusted input: unrecoverable text yields `None` or an empty object.

use serde_json::{Map, Number, Value};

/// A JSON value with object members kept in insertion order.
///
/// `serde_json::Value` sorts object keys because its map is a `BTreeMap` in this
/// build. Keeping the order here lets `repair_partial_json` return text that
/// follows the input, while `parse_partial_json` still returns a plain `Value`.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

impl Node {
    /// Converts the node into a plain `serde_json::Value`.
    fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(items) => Value::Array(items.into_iter().map(Self::into_value).collect()),
            Self::Object(members) => {
                let mut map = Map::with_capacity(members.len());
                for (key, value) in members {
                    map.insert(key, value.into_value());
                }
                Value::Object(map)
            }
        }
    }

    /// Writes the node as valid JSON, preserving member order.
    fn write_json(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Number(number) => out.push_str(&number.to_string()),
            Self::String(text) => {
                if let Ok(encoded) = serde_json::to_string(text) {
                    out.push_str(&encoded);
                }
            }
            Self::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            Self::Object(members) => {
                out.push('{');
                for (index, (key, value)) in members.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    if let Ok(encoded) = serde_json::to_string(key) {
                        out.push_str(&encoded);
                    }
                    out.push(':');
                    value.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// Where a container being opened will be attached once it closes.
#[derive(Debug)]
enum Slot {
    Root,
    Array,
    Member(String),
    Invalid,
}

/// What an object frame expects next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectPhase {
    Key,
    Colon,
    Value,
    Comma,
}

/// An open array or object. A container is attached to its parent only when it
/// closes, which keeps nested containers in the right order even when the text
/// is cut off.
#[derive(Debug)]
enum Frame {
    Array {
        items: Vec<Node>,
        slot: Slot,
    },
    Object {
        members: Vec<(String, Node)>,
        key: Option<String>,
        phase: ObjectPhase,
        slot: Slot,
    },
}

/// Inserts a member, keeping the last value at the first position of a key.
fn insert_member(members: &mut Vec<(String, Node)>, key: String, value: Node) {
    if let Some(entry) = members.iter_mut().find(|(existing, _)| *existing == key) {
        entry.1 = value;
    } else {
        members.push((key, value));
    }
}

/// Characters that end a number or literal token outside strings.
fn is_delimiter(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | ',' | ':' | '{' | '}' | '[' | ']' | '"'
    )
}

/// Parses four hexadecimal digits into a code point.
fn hex_value(digits: &str) -> Option<u32> {
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// The value of a complete or cut-off number token.
///
/// A trailing exponent without digits is ignored (`2.5e` reads as `2.5`), but a
/// dot without digits leaves no value at all (`12.` reads as nothing).
fn number_value(token: &str) -> Option<Node> {
    let chars: Vec<char> = token.chars().collect();
    let mut index = 0;
    if chars.first() == Some(&'-') {
        index = 1;
    }
    let digits_start = index;
    while index < chars.len() && chars[index].is_ascii_digit() {
        index += 1;
    }
    if index == digits_start {
        return None;
    }

    let mut has_fraction = false;
    if index < chars.len() && chars[index] == '.' {
        let fraction_start = index + 1;
        index = fraction_start;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if index == fraction_start {
            return None;
        }
        has_fraction = true;
    }

    let mut text: String = chars[..index].iter().collect();
    if index < chars.len() && (chars[index] == 'e' || chars[index] == 'E') {
        index += 1;
        let mut exponent = String::new();
        if index < chars.len() && (chars[index] == '+' || chars[index] == '-') {
            exponent.push(chars[index]);
            index += 1;
        }
        let exponent_start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            exponent.push(chars[index]);
            index += 1;
        }
        if index > exponent_start {
            text.push('e');
            text.push_str(&exponent);
        }
    }
    if index != chars.len() {
        return None;
    }

    let integer_value = if has_fraction {
        None
    } else {
        text.parse::<i64>().ok()
    };
    if let Some(integer) = integer_value {
        return Some(Node::Number(Number::from(integer)));
    }
    let float = text.parse::<f64>().ok()?;
    if !float.is_finite() {
        return None;
    }
    Number::from_f64(float).map(Node::Number)
}

/// The value of a complete token or a prefix of one of the JSON literals.
fn scalar_value(token: &str) -> Option<Node> {
    for (literal, value) in [
        ("true", Node::Bool(true)),
        ("false", Node::Bool(false)),
        ("null", Node::Null),
    ] {
        if literal.starts_with(token) {
            return Some(value);
        }
    }
    number_value(token)
}

/// Removes a Markdown code fence around JSON, including an optional language.
fn strip_markdown_fence(input: &str) -> &str {
    let mut text = input.trim();
    if !text.starts_with("```") {
        return text;
    }
    text = text[3..].trim_start();

    if text
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic())
    {
        let tag_len = text.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
        let tag = &text[..tag_len];
        const LANGUAGES: [&str; 15] = [
            "json",
            "jsonc",
            "json5",
            "js",
            "javascript",
            "ts",
            "typescript",
            "text",
            "yaml",
            "yml",
            "python",
            "bash",
            "shell",
            "toml",
            "ini",
        ];
        if LANGUAGES.iter().any(|name| tag.eq_ignore_ascii_case(name)) {
            text = text[tag_len..].trim_start();
        }
    }

    if let Some(end) = text.rfind("```") {
        text = &text[..end];
    }
    text.trim()
}

/// A tolerant, single-pass parser. Strings and numbers are decoded while they
/// are read; structural mistakes are ignored instead of failing the parse.
struct Parser {
    chars: Vec<char>,
    pos: usize,
    stack: Vec<Frame>,
    root: Option<Node>,
    done: bool,
}

impl Parser {
    fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            pos: 0,
            stack: Vec::new(),
            root: None,
            done: false,
        }
    }

    /// Parses all available text, then closes whatever is still open.
    fn parse(mut self) -> Option<Node> {
        while self.pos < self.chars.len() && !self.done {
            let c = self.chars[self.pos];
            match c {
                '"' => {
                    self.pos += 1;
                    let text = self.read_string();
                    self.handle_string(text);
                }
                '{' | '[' => {
                    self.pos += 1;
                    self.open_container(c == '{');
                }
                '}' | ']' => {
                    self.pos += 1;
                    self.close_container();
                }
                ':' => {
                    self.pos += 1;
                    self.handle_colon();
                }
                ',' => {
                    self.pos += 1;
                    self.handle_comma();
                }
                _ if is_delimiter(c) => {
                    self.pos += 1;
                }
                _ => {
                    let start = self.pos;
                    while self.pos < self.chars.len() && !is_delimiter(self.chars[self.pos]) {
                        self.pos += 1;
                    }
                    let token: String = self.chars[start..self.pos].iter().collect();
                    match scalar_value(&token) {
                        Some(node) => self.place_value(node),
                        None => self.value_ended(),
                    }
                }
            }
        }
        while !self.stack.is_empty() {
            self.close_container();
        }
        self.root
    }

    /// Reads a string from the current position until its quote or end of text.
    fn read_string(&mut self) -> String {
        let mut out = String::new();
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            self.pos += 1;
            match c {
                '"' => break,
                '\\' => {
                    let Some(&next) = self.chars.get(self.pos) else {
                        // An escape cut off by the end of the stream is held back.
                        break;
                    };
                    match next {
                        '"' => {
                            out.push('"');
                            self.pos += 1;
                        }
                        '\\' => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        '/' => {
                            out.push('/');
                            self.pos += 1;
                        }
                        'b' => {
                            out.push('\u{0008}');
                            self.pos += 1;
                        }
                        'f' => {
                            out.push('\u{000c}');
                            self.pos += 1;
                        }
                        'n' => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        'r' => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        't' => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        'u' => {
                            let digits_start = self.pos + 1;
                            if digits_start + 4 > self.chars.len() {
                                // Incomplete `\u`: drop it and the digits read so far.
                                break;
                            }
                            let digits: String =
                                self.chars[digits_start..digits_start + 4].iter().collect();
                            if let Some(first) = hex_value(&digits) {
                                self.pos = digits_start + 4;
                                self.push_unicode(first, &mut out);
                            } else {
                                // An invalid escape stays literal text.
                                out.push('\\');
                            }
                        }
                        _ => {
                            // An invalid escape stays literal text.
                            out.push('\\');
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// Pushes a decoded `\u` code point, joining a surrogate pair when present.
    fn push_unicode(&mut self, first: u32, out: &mut String) {
        if !(0xd800..=0xdbff).contains(&first) {
            if (0xdc00..=0xdfff).contains(&first) {
                // A lone low surrogate cannot be represented in a Rust string.
                return;
            }
            if let Some(c) = char::from_u32(first) {
                out.push(c);
            }
            return;
        }
        let Some(tail) = self.chars.get(self.pos..self.pos.saturating_add(6)) else {
            return;
        };
        if tail[0] != '\\' || tail[1] != 'u' {
            return;
        }
        let digits: String = tail[2..6].iter().collect();
        let pair = hex_value(&digits)
            .filter(|low| (0xdc00..=0xdfff).contains(low))
            .and_then(|low| char::from_u32(0x1_0000 + ((first - 0xd800) << 10) + (low - 0xdc00)));
        if let Some(c) = pair {
            out.push(c);
            self.pos += 6;
        }
        // A lone high surrogate cannot be represented in a Rust string.
    }

    /// Records a string as an object key when one is expected, else as a value.
    fn handle_string(&mut self, text: String) {
        let is_key = matches!(
            self.stack.last(),
            Some(Frame::Object { phase, .. })
                if *phase == ObjectPhase::Key || *phase == ObjectPhase::Comma
        );
        if is_key {
            let Some(Frame::Object { key, phase, .. }) = self.stack.last_mut() else {
                return;
            };
            *key = Some(text);
            *phase = ObjectPhase::Colon;
            return;
        }
        self.place_value(Node::String(text));
    }

    /// Places a completed value into the current slot.
    fn place_value(&mut self, node: Node) {
        match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    self.root = Some(node);
                }
                self.done = true;
            }
            Some(Frame::Array { items, .. }) => items.push(node),
            Some(Frame::Object {
                members,
                key,
                phase,
                ..
            }) => {
                if (*phase == ObjectPhase::Value || *phase == ObjectPhase::Colon)
                    && let Some(name) = key.take()
                {
                    insert_member(members, name, node);
                }
                *phase = ObjectPhase::Comma;
            }
        }
    }

    /// Marks the current value as ended without placing one (invalid token).
    fn value_ended(&mut self) {
        match self.stack.last_mut() {
            None => {
                if self.root.is_some() {
                    self.done = true;
                }
            }
            Some(Frame::Object { phase, .. }) => *phase = ObjectPhase::Comma,
            Some(Frame::Array { .. }) => {}
        }
    }

    /// Opens an array or object and records where it will be attached.
    fn open_container(&mut self, is_object: bool) {
        let slot = match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    Slot::Root
                } else {
                    Slot::Invalid
                }
            }
            Some(Frame::Array { .. }) => Slot::Array,
            Some(Frame::Object { key, phase, .. }) => {
                if (*phase == ObjectPhase::Value || *phase == ObjectPhase::Colon)
                    && let Some(name) = key.take()
                {
                    Slot::Member(name)
                } else {
                    Slot::Invalid
                }
            }
        };
        if let Some(Frame::Object { phase, .. }) = self.stack.last_mut() {
            *phase = ObjectPhase::Comma;
        }
        let frame = if is_object {
            Frame::Object {
                members: Vec::new(),
                key: None,
                phase: ObjectPhase::Key,
                slot,
            }
        } else {
            Frame::Array {
                items: Vec::new(),
                slot,
            }
        };
        self.stack.push(frame);
    }

    /// Closes the innermost container, attaching it to its parent.
    fn close_container(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        let (node, slot) = match frame {
            Frame::Array { items, slot } => (Node::Array(items), slot),
            Frame::Object { members, slot, .. } => (Node::Object(members), slot),
        };
        match slot {
            Slot::Root => {
                if self.root.is_none() {
                    self.root = Some(node);
                }
            }
            Slot::Array => {
                if let Some(Frame::Array { items, .. }) = self.stack.last_mut() {
                    items.push(node);
                }
            }
            Slot::Member(name) => {
                if let Some(Frame::Object { members, .. }) = self.stack.last_mut() {
                    insert_member(members, name, node);
                }
            }
            Slot::Invalid => {}
        }
        self.value_ended();
    }

    fn handle_colon(&mut self) {
        if let Some(Frame::Object { phase, .. }) = self.stack.last_mut()
            && *phase == ObjectPhase::Colon
        {
            *phase = ObjectPhase::Value;
        }
    }

    fn handle_comma(&mut self) {
        if let Some(Frame::Object { phase, .. }) = self.stack.last_mut() {
            *phase = ObjectPhase::Key;
        }
    }
}

/// Parses JSON that may be truncated or mildly malformed.
///
/// Returns the value read so far, including completed members and elements of
/// still-open containers. Returns `None` when no value can be recovered, for
/// empty text or for a number cut after its decimal point.
pub fn parse_partial_json(input: &str) -> Option<Value> {
    let text = strip_markdown_fence(input);
    if text.is_empty() {
        return None;
    }
    Parser::new(text).parse().map(Node::into_value)
}

/// Repairs truncated or malformed JSON text into valid JSON.
///
/// Strings are closed and their escapes fixed, open containers are closed,
/// trailing commas are dropped, and incomplete members are omitted. Input that
/// is already valid JSON is returned unchanged. Text with no recoverable value
/// becomes `{}`.
pub fn repair_partial_json(input: &str) -> String {
    let text = strip_markdown_fence(input);
    if text.is_empty() {
        return "{}".to_string();
    }
    if serde_json::from_str::<Value>(text).is_ok() {
        return text.to_string();
    }
    match Parser::new(text).parse() {
        Some(node) => {
            let mut out = String::new();
            node.write_json(&mut out);
            out
        }
        None => "{}".to_string(),
    }
}

/// Parses the argument text of a finished tool call into an object.
///
/// Empty text, fenced JSON, truncated objects, and repairable malformed text
/// all produce an object; text that is not or cannot be repaired into an object
/// produces `{}`.
pub fn parse_tool_arguments(raw: &str) -> Value {
    let text = strip_markdown_fence(raw);
    if text.is_empty() {
        return Value::Object(Map::new());
    }
    let repaired = repair_partial_json(text);
    match serde_json::from_str::<Value>(&repaired) {
        Ok(value @ Value::Object(_)) => value,
        _ => Value::Object(Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_partial_json, parse_tool_arguments, repair_partial_json};
    use serde_json::{Value, json};

    #[test]
    fn parses_complete_values_exactly() {
        let cases = [
            ("", None),
            ("null", Some(json!(null))),
            ("true", Some(json!(true))),
            ("-1.25e3", Some(json!(-1250.0))),
            ("\"text\"", Some(json!("text"))),
            (
                "[1, \"two\", {\"k\": false}]",
                Some(json!([1, "two", {"k": false}])),
            ),
            (
                "{\"a\": {\"b\": [1, 2]}}",
                Some(json!({"a": {"b": [1, 2]}})),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_partial_json(input), expected, "input: {input}");
        }
    }

    #[test]
    fn closes_truncated_strings_and_decodes_escapes() {
        assert_eq!(parse_partial_json("\"abc"), Some(json!("abc")));
        assert_eq!(
            parse_partial_json(r#"{"a":"x\u00e9y"}"#),
            Some(json!({"a": "xéy"}))
        );
        assert_eq!(
            parse_partial_json(r#"{"a":"x\u00"#),
            Some(json!({"a": "x"}))
        );
        assert_eq!(
            parse_partial_json(r#"{"a":"back\url{x}"}"#),
            Some(json!({"a": "back\\url{x}"}))
        );
        assert_eq!(
            parse_partial_json(r#"{"a":"x\ny"}"#),
            Some(json!({"a": "x\ny"}))
        );
        assert_eq!(
            parse_partial_json(r#"{"a":"\ud83d\ude00!"}"#),
            Some(json!({"a": "😀!"}))
        );
        assert_eq!(
            parse_partial_json("\"trailing \\"),
            Some(json!("trailing "))
        );
    }

    #[test]
    fn closes_truncated_containers_and_drops_trailing_commas() {
        assert_eq!(parse_partial_json("["), Some(json!([])));
        assert_eq!(parse_partial_json("[1, 2,"), Some(json!([1, 2])));
        assert_eq!(
            parse_partial_json("[[1], [2, 3, ["),
            Some(json!([[1], [2, 3, []]]))
        );
        assert_eq!(parse_partial_json("{\"a\":1,}"), Some(json!({"a": 1})));
        assert_eq!(
            parse_partial_json("{\"a\": {\"b\": {"),
            Some(json!({"a": {"b": {}}}))
        );
        assert_eq!(
            parse_partial_json("{\"a\": 1, \"b\": [1, 2, {"),
            Some(json!({"a": 1, "b": [1, 2, {}]}))
        );
    }

    #[test]
    fn omits_incomplete_members_and_completes_prefixes() {
        assert_eq!(parse_partial_json("{\"key\":",), Some(json!({})));
        assert_eq!(
            parse_partial_json("{\"k1\": 1, \"k2"),
            Some(json!({"k1": 1}))
        );
        assert_eq!(
            parse_partial_json("{\"a\": 1, \"b\": \""),
            Some(json!({"a": 1, "b": ""}))
        );
        assert_eq!(parse_partial_json("nul"), Some(json!(null)));
        assert_eq!(parse_partial_json("tru"), Some(json!(true)));
        assert_eq!(parse_partial_json("2.5e"), Some(json!(2.5)));
        assert_eq!(parse_partial_json("12."), None);
        assert_eq!(parse_partial_json("-"), None);
    }

    #[test]
    fn tolerates_malformed_text_and_trailing_garbage() {
        assert_eq!(
            parse_partial_json("{\"a\": 1 \"b\": 2}"),
            Some(json!({"a": 1, "b": 2}))
        );
        assert_eq!(parse_partial_json("{\"a\" 1}"), Some(json!({"a": 1})));
        assert_eq!(parse_partial_json("]}}}{"), Some(json!({})));
        assert_eq!(
            parse_partial_json("{\"a\": tru, \"b\": nulll, \"c\": falsey}"),
            Some(json!({"a": true}))
        );
        assert_eq!(
            parse_partial_json("{\"a\":1} trailing {\"b\":2}"),
            Some(json!({"a": 1}))
        );
        assert_eq!(parse_partial_json("NaN"), None);
    }

    #[test]
    fn repair_produces_parseable_json() {
        let cases = [
            ("{\"a\":1,}", "{\"a\":1}"),
            ("[1,2,]", "[1,2]"),
            ("{\"a\":\"unterminated", "{\"a\":\"unterminated\"}"),
            ("{\"a\":1", "{\"a\":1}"),
            ("{\"b\":1,\"a\":2", "{\"b\":1,\"a\":2}"),
            ("", "{}"),
        ];
        for (input, expected) in cases {
            let repaired = repair_partial_json(input);
            assert_eq!(repaired, expected, "input: {input}");
            let parsed: Value = serde_json::from_str(&repaired).expect("repaired JSON parses");
            assert!(parsed.is_object() || parsed.is_array());
        }
        assert_eq!(repair_partial_json("  {\"a\": 1}  "), "{\"a\": 1}");
    }

    #[test]
    fn tool_arguments_handle_empty_fences_and_truncation() {
        assert_eq!(parse_tool_arguments(""), json!({}));
        assert_eq!(parse_tool_arguments("   "), json!({}));
        assert_eq!(
            parse_tool_arguments("```json\n{\"a\":1}\n```"),
            json!({"a": 1})
        );
        assert_eq!(parse_tool_arguments("```{\"a\":1}```"), json!({"a": 1}));
        assert_eq!(parse_tool_arguments("```JSON\n{\"a\":1}"), json!({"a": 1}));
        assert_eq!(
            parse_tool_arguments("{\"path\":\"a.ts\",\"content\":\"xy"),
            json!({"path": "a.ts", "content": "xy"})
        );
        assert_eq!(
            parse_tool_arguments(r#"{"kind":"seek","note":"\url"}"#),
            json!({"kind": "seek", "note": "\\url"})
        );
    }

    #[test]
    fn tool_arguments_reject_non_objects() {
        assert_eq!(parse_tool_arguments("[1,2]"), json!({}));
        assert_eq!(parse_tool_arguments("\"text\""), json!({}));
        assert_eq!(parse_tool_arguments("42"), json!({}));
    }

    #[test]
    fn parse_partial_json_strips_fences_itself() {
        assert_eq!(
            parse_partial_json("```json\n{\"a\":1}\n```"),
            Some(json!({"a": 1}))
        );
        assert_eq!(parse_partial_json("```\n[1,2\n```"), Some(json!([1, 2])));
    }
}
