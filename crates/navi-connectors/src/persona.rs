//! Persona definitions and TTS text cleanup, ported from `apps/navi/workers/persona.py`.
//!
//! Python's `re` is unavailable here, so each substitution is matched by hand over a slice of
//! Unicode scalar values (Python string indices count code points). Greedy quantifiers try their
//! longest match first, which is the order in which the Python engine backtracks.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const NAVI_PROMPT: &str = "You are Navi, the user's assistant on their own computer. Your name is Navi; that is your only name. Answer directly and concisely — one to three short sentences unless the user asks for detail. Plain prose: no markdown, no code blocks, no emoji, because your replies may be spoken aloud. You run inside the Navi desktop app and OpenCode provides your tools; use them when a request needs real work on the machine.";
const DEFAULT_PERSONA_NAME: &str = "navi";
const DEFAULT_VOICE: &str = "en-GB-RyanNeural";
const DEFAULT_RATE: &str = "+0%";

const SHORT_FORMS: [&str; 10] = [
    "e.g.", "i.e.", "etc.", "vs.", "mr.", "mrs.", "ms.", "dr.", "no.", "approx.",
];

/// A voice persona: its identity, TTS hints, and the system prompt sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persona {
    pub name: String,
    pub voice: String,
    pub rate: String,
    pub system_prompt: String,
}

struct PersonaSpec {
    name: &'static str,
    voice: &'static str,
    rate: &'static str,
    system_prompt: &'static str,
}

static BUILTIN_PERSONAS: &[PersonaSpec] = &[PersonaSpec {
    name: DEFAULT_PERSONA_NAME,
    voice: DEFAULT_VOICE,
    rate: DEFAULT_RATE,
    system_prompt: NAVI_PROMPT,
}];

fn spec_to_persona(spec: &PersonaSpec) -> Persona {
    Persona {
        name: spec.name.to_string(),
        voice: spec.voice.to_string(),
        rate: spec.rate.to_string(),
        system_prompt: spec.system_prompt.to_string(),
    }
}

fn builtin_persona(name: &str) -> Option<Persona> {
    BUILTIN_PERSONAS
        .iter()
        .find(|spec| spec.name == name)
        .map(spec_to_persona)
}

/// Returns the built-in personas, in the order defined by the Python `BUILTIN` table.
pub fn default_personas() -> Vec<Persona> {
    BUILTIN_PERSONAS.iter().map(spec_to_persona).collect()
}

/// Returns every known persona name, sorted.
pub fn persona_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = BUILTIN_PERSONAS.iter().map(|spec| spec.name).collect();
    names.sort_unstable();
    names
}

/// Maps a voice-command alias to the persona name it selects. Navi is the only identity.
pub fn aliases() -> BTreeMap<&'static str, &'static str> {
    BUILTIN_PERSONAS
        .iter()
        .map(|spec| (spec.name, spec.name))
        .collect()
}

/// Resolves a persona name, trimming and lowercasing it as the Python voice command does.
/// Unknown names select the default Navi persona.
pub fn resolve_persona(name: &str) -> Persona {
    let key = name.trim().to_lowercase();
    builtin_persona(&key)
        .or_else(|| builtin_persona(DEFAULT_PERSONA_NAME))
        .unwrap_or_else(|| Persona {
            name: DEFAULT_PERSONA_NAME.to_string(),
            voice: DEFAULT_VOICE.to_string(),
            rate: DEFAULT_RATE.to_string(),
            system_prompt: NAVI_PROMPT.to_string(),
        })
}

/// A match: start, capture start, capture end, end. Matches without a group use `(0, 0)` for the
/// capture, so a replacement callback ignores it.
type Match = (usize, usize, usize, usize);

// --- Python string semantics -------------------------------------------------

/// Python `re`'s `\s`: the Unicode White_Space set plus the four information separators, which
/// Rust's `char::is_whitespace` leaves out.
fn is_py_space(c: char) -> bool {
    matches!(c,
        '\u{0009}'..='\u{000D}' | '\u{001C}'..='\u{001F}' | '\u{0020}' | '\u{0085}' |
        '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' |
        '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// Python `re`'s `\w`: Unicode alphanumeric characters plus the underscore.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `[\w.+-]` from the `_PATH` pattern.
fn is_path_char(c: char) -> bool {
    is_word(c) || c == '.' || c == '+' || c == '-'
}

/// `[\w+-]` from the `_PATH` lookahead.
fn is_short_char(c: char) -> bool {
    is_word(c) || c == '+' || c == '-'
}

/// Python `re`'s `\d`.
fn is_digit(c: char) -> bool {
    c.is_numeric()
}

fn is_url_stop(c: char) -> bool {
    is_py_space(c) || matches!(c, ')' | ']' | '>' | '"' | '\'')
}

fn is_emoji(c: char) -> bool {
    let u = c as u32;
    (0x1F000..=0x1FAFF).contains(&u) || (0x2600..=0x27BF).contains(&u) || u == 0xFE0F || u == 0x200D
}

fn trim_chars(chars: &[char]) -> &[char] {
    let mut start = 0;
    let mut end = chars.len();
    while start < end && is_py_space(chars[start]) {
        start += 1;
    }
    while end > start && is_py_space(chars[end - 1]) {
        end -= 1;
    }
    &chars[start..end]
}

fn find_subslice(text: &[char], pos: usize, pat: &[char]) -> Option<usize> {
    if pat.is_empty() || pos + pat.len() > text.len() {
        return None;
    }
    (pos..=text.len() - pat.len()).find(|&i| text[i..i + pat.len()].iter().eq(pat.iter()))
}

fn starts_with(text: &[char], pos: usize, pat: &str) -> bool {
    (pos..)
        .zip(pat.chars())
        .all(|(i, c)| text.get(i) == Some(&c))
}

/// `re.sub`: replace every non-overlapping match found at or after the end of the previous one.
fn replace_all(
    text: &[char],
    mut find: impl FnMut(&[char], usize) -> Option<Match>,
    mut replacement: impl FnMut(&[char], Match) -> Vec<char>,
) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut pos = 0;
    while pos <= text.len() {
        let Some(m) = find(text, pos) else {
            break;
        };
        let (start, _, _, end) = m;
        if start < pos || end <= start {
            break;
        }
        out.extend_from_slice(&text[pos..start]);
        out.extend(replacement(text, m));
        pos = end;
    }
    out.extend_from_slice(&text[pos..]);
    out
}

/// The next `^` position at or after `pos` under `re.M`: index zero or just after a newline.
fn next_line_start(text: &[char], pos: usize) -> Option<usize> {
    if pos == 0 || (pos <= text.len() && text.get(pos.wrapping_sub(1)) == Some(&'\n')) {
        return Some(pos);
    }
    let rel = text.get(pos..)?.iter().position(|&c| c == '\n')?;
    Some(pos + rel + 1)
}

fn find_on_lines(
    text: &[char],
    pos: usize,
    mut at_line: impl FnMut(&[char], usize) -> Option<Match>,
) -> Option<Match> {
    let mut line = next_line_start(text, pos)?;
    loop {
        if let Some(m) = at_line(text, line) {
            return Some(m);
        }
        let rel = text.get(line..)?.iter().position(|&c| c == '\n')?;
        line += rel + 1;
    }
}

// --- Individual substitutions, in the order `sanitize_for_tts` applies them --

/// `^\s*(?:PROGRESS|TASK)\s.*$` with `re.M`.
fn find_progress(text: &[char], pos: usize) -> Option<Match> {
    find_on_lines(text, pos, |text, line| {
        let n = text.len();
        let mut j = line;
        while j < n && is_py_space(text[j]) {
            j += 1;
        }
        let mut k = j;
        loop {
            let plen = if starts_with(text, k, "PROGRESS") {
                Some(8)
            } else if starts_with(text, k, "TASK") {
                Some(4)
            } else {
                None
            };
            if let Some(plen) = plen {
                let after = k + plen;
                if after < n && is_py_space(text[after]) {
                    let mut end = after + 1;
                    while end < n && text[end] != '\n' {
                        end += 1;
                    }
                    return Some((line, 0, 0, end));
                }
            }
            if k == line {
                break;
            }
            k -= 1;
        }
        None
    })
}

/// `` ```.*?``` `` with `re.S`.
fn find_fenced(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i + 3 <= n {
        if starts_with(text, i, "```")
            && let Some(rel) = find_subslice(text, i + 3, &['`', '`', '`'])
        {
            return Some((i, 0, 0, rel + 3));
        }
        i += 1;
    }
    None
}

/// `` ```.* `` with `re.S`: a fence left after the paired pass runs to the end.
fn find_unterminated_fence(text: &[char], pos: usize) -> Option<Match> {
    let start = find_subslice(text, pos, &['`', '`', '`'])?;
    Some((start, 0, 0, text.len()))
}

/// `` `([^`]*)` ``.
fn find_inline_code(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i < n && text[i] != '`' {
        i += 1;
    }
    if i == n {
        return None;
    }
    let mut j = i + 1;
    while j < n && text[j] != '`' {
        j += 1;
    }
    if j == n {
        return None;
    }
    Some((i, i + 1, j, j + 1))
}

/// `^\s{0,3}#{1,6}\s*` with `re.M`.
fn find_heading(text: &[char], pos: usize) -> Option<Match> {
    find_on_lines(text, pos, |text, line| {
        let n = text.len();
        for ws in (0..=3).rev() {
            let p = line + ws;
            if p > n || !text[line..p].iter().all(|&c| is_py_space(c)) {
                continue;
            }
            for hashes in (1..=6).rev() {
                let q = p + hashes;
                if q > n || !text[p..q].iter().all(|&c| c == '#') {
                    continue;
                }
                let mut end = q;
                while end < n && is_py_space(text[end]) {
                    end += 1;
                }
                return Some((line, 0, 0, end));
            }
        }
        None
    })
}

/// `\*\*?([^*]+)\*\*?`.
fn find_bold(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i < n {
        if text[i] == '*' {
            for open in [2, 1] {
                if i + open > n || !text[i..i + open].iter().all(|&c| c == '*') {
                    continue;
                }
                let cap_start = i + open;
                let mut cap_end = cap_start;
                while cap_end < n && text[cap_end] != '*' {
                    cap_end += 1;
                }
                if cap_end == cap_start {
                    continue;
                }
                for close in [2, 1] {
                    if cap_end + close <= n
                        && text[cap_end..cap_end + close].iter().all(|&c| c == '*')
                    {
                        return Some((i, cap_start, cap_end, cap_end + close));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// `__(...)__` or `~~(...)~~`.
fn find_double_delim(text: &[char], pos: usize, delim: char) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i + 1 < n {
        if text[i] == delim && text[i + 1] == delim {
            let cap_start = i + 2;
            let mut cap_end = cap_start;
            while cap_end < n && text[cap_end] != delim {
                cap_end += 1;
            }
            if cap_end > cap_start
                && cap_end + 1 < n
                && text[cap_end] == delim
                && text[cap_end + 1] == delim
            {
                return Some((i, cap_start, cap_end, cap_end + 2));
            }
        }
        i += 1;
    }
    None
}

/// `^\s*[-*+]\s+` with `re.M`.
fn find_bullet(text: &[char], pos: usize) -> Option<Match> {
    find_on_lines(text, pos, |text, line| {
        let n = text.len();
        let mut j = line;
        while j < n && is_py_space(text[j]) {
            j += 1;
        }
        for k in (line..=j).rev() {
            if k < n && matches!(text[k], '-' | '*' | '+') && k + 1 < n && is_py_space(text[k + 1])
            {
                let mut end = k + 1;
                while end < n && is_py_space(text[end]) {
                    end += 1;
                }
                return Some((line, 0, 0, end));
            }
        }
        None
    })
}

/// `^\s*\d+\.\s+` with `re.M`.
fn find_numbered(text: &[char], pos: usize) -> Option<Match> {
    find_on_lines(text, pos, |text, line| {
        let n = text.len();
        let mut j = line;
        while j < n && is_py_space(text[j]) {
            j += 1;
        }
        for k in (line..=j).rev() {
            let mut digits_end = k;
            while digits_end < n && is_digit(text[digits_end]) {
                digits_end += 1;
            }
            while digits_end > k {
                if digits_end < n && text[digits_end] == '.' {
                    let mut end = digits_end + 1;
                    if end < n && is_py_space(text[end]) {
                        while end < n && is_py_space(text[end]) {
                            end += 1;
                        }
                        return Some((line, 0, 0, end));
                    }
                }
                digits_end -= 1;
            }
        }
        None
    })
}

/// `\[([^\]]+)\]\([^)]*\)`.
fn find_link(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i < n {
        if text[i] == '[' {
            let cap_start = i + 1;
            let mut cap_end = cap_start;
            while cap_end < n && text[cap_end] != ']' {
                cap_end += 1;
            }
            if cap_end > cap_start
                && cap_end + 1 < n
                && text[cap_end] == ']'
                && text[cap_end + 1] == '('
            {
                let mut end = cap_end + 2;
                while end < n && text[end] != ')' {
                    end += 1;
                }
                if end < n {
                    return Some((i, cap_start, cap_end, end + 1));
                }
            }
        }
        i += 1;
    }
    None
}

/// `!\[[^\]]*\]\([^)]*\)`.
fn find_image(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i + 1 < n {
        if text[i] == '!' && text[i + 1] == '[' {
            let mut j = i + 2;
            while j < n && text[j] != ']' {
                j += 1;
            }
            if j + 1 < n && text[j] == ']' && text[j + 1] == '(' {
                let mut end = j + 2;
                while end < n && text[end] != ')' {
                    end += 1;
                }
                if end < n {
                    return Some((i, 0, 0, end + 1));
                }
            }
        }
        i += 1;
    }
    None
}

/// `^\s*>\s?` with `re.M`.
fn find_quote(text: &[char], pos: usize) -> Option<Match> {
    find_on_lines(text, pos, |text, line| {
        let n = text.len();
        let mut j = line;
        while j < n && is_py_space(text[j]) {
            j += 1;
        }
        for k in (line..=j).rev() {
            if k < n && text[k] == '>' {
                let mut end = k + 1;
                if end < n && is_py_space(text[end]) {
                    end += 1;
                }
                return Some((line, 0, 0, end));
            }
        }
        None
    })
}

/// `^\s*[-*_]{3,}\s*$` with `re.M`.
fn find_rule(text: &[char], pos: usize) -> Option<Match> {
    find_on_lines(text, pos, |text, line| {
        let n = text.len();
        let mut j = line;
        while j < n && is_py_space(text[j]) {
            j += 1;
        }
        for k in (line..=j).rev() {
            let mut d = k;
            while d < n && matches!(text[d], '-' | '*' | '_') {
                d += 1;
            }
            let mut count = d - k;
            while count >= 3 {
                let marks_end = k + count;
                let mut e = marks_end;
                while e < n && is_py_space(text[e]) {
                    e += 1;
                }
                // Backtrack the trailing `\s*` until `$` (end of string or before a newline)
                // holds; greedy takes the largest such position.
                let mut candidate = e;
                while candidate > marks_end && candidate != n && text[candidate] != '\n' {
                    candidate -= 1;
                }
                if candidate == n || (candidate < n && text[candidate] == '\n') {
                    return Some((line, 0, 0, candidate));
                }
                count -= 1;
            }
        }
        None
    })
}

/// `[ \t]+` collapsed to a single space.
fn collapse_hspace(text: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if matches!(text[i], ' ' | '\t') {
            out.push(' ');
            while i < text.len() && matches!(text[i], ' ' | '\t') {
                i += 1;
            }
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

/// `(?:https?://|www\.)[^\s)\]>\"']+`.
fn find_url(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i < n {
        let prefix = if starts_with(text, i, "https://") {
            8
        } else if starts_with(text, i, "http://") {
            7
        } else if starts_with(text, i, "www.") {
            4
        } else {
            0
        };
        if prefix > 0 {
            let mut end = i + prefix;
            while end < n && !is_url_stop(text[end]) {
                end += 1;
            }
            if end > i + prefix {
                return Some((i, 0, 0, end));
            }
        }
        i += 1;
    }
    None
}

/// The greedy `(?:[\w.+-]+/)*([\w.+-]+)` tail shared by both `_PATH` alternatives.
fn match_path_tail(text: &[char], pos: usize) -> Option<(usize, usize, usize)> {
    let n = text.len();
    let mut starts = vec![pos];
    let mut seg = pos;
    loop {
        let mut q = seg;
        while q < n && is_path_char(text[q]) {
            q += 1;
        }
        if q == seg {
            break;
        }
        if q < n && text[q] == '/' {
            seg = q + 1;
            starts.push(seg);
        } else {
            break;
        }
    }
    for &cap_start in starts.iter().rev() {
        let mut cap_end = cap_start;
        while cap_end < n && is_path_char(text[cap_end]) {
            cap_end += 1;
        }
        if cap_end > cap_start {
            return Some((cap_start, cap_end, cap_end));
        }
    }
    None
}

/// `(?=[\w+-]+\.\w)` from the second `_PATH` alternative.
fn has_extension(text: &[char], pos: usize) -> bool {
    let n = text.len();
    let mut q = pos;
    while q < n && is_short_char(text[q]) {
        q += 1;
    }
    q > pos && q < n && text[q] == '.' && q + 1 < n && is_word(text[q + 1])
}

fn match_path_at(text: &[char], i: usize) -> Option<Match> {
    let n = text.len();

    // Alternative A: (?<![\w/.~])(?:~|\.{1,2})?/(?:[\w.+-]+/)*([\w.+-]+)
    let lookbehind_ok = i == 0 || {
        let p = text[i - 1];
        !is_word(p) && p != '/' && p != '.' && p != '~'
    };
    if lookbehind_ok {
        let mut prefixes = Vec::new();
        if i < n && text[i] == '~' {
            prefixes.push(1);
        } else if i < n && text[i] == '.' {
            if i + 1 < n && text[i + 1] == '.' {
                prefixes.push(2);
            }
            prefixes.push(1);
        }
        prefixes.push(0);
        for plen in prefixes {
            let p = i + plen;
            if p < n
                && text[p] == '/'
                && let Some((cs, ce, end)) = match_path_tail(text, p + 1)
            {
                return Some((i, cs, ce, end));
            }
        }
    }

    // Alternative B: \b(?:[\w.+-]+/)+(?=[\w+-]+\.\w)([\w.+-]+)
    let left_word = i > 0 && is_word(text[i - 1]);
    let right_word = i < n && is_word(text[i]);
    if left_word != right_word {
        let mut ends = Vec::new();
        let mut seg = i;
        loop {
            let mut q = seg;
            while q < n && is_path_char(text[q]) {
                q += 1;
            }
            if q == seg {
                break;
            }
            if q < n && text[q] == '/' {
                seg = q + 1;
                ends.push(seg);
            } else {
                break;
            }
        }
        for &cap_start in ends.iter().rev() {
            if has_extension(text, cap_start) {
                let mut cap_end = cap_start;
                while cap_end < n && is_path_char(text[cap_end]) {
                    cap_end += 1;
                }
                if cap_end > cap_start {
                    return Some((i, cap_start, cap_end, cap_end));
                }
            }
        }
    }
    None
}

fn find_path(text: &[char], pos: usize) -> Option<Match> {
    (pos..text.len()).find_map(|i| match_path_at(text, i))
}

/// `\n\s*\n+`: the match ends after the last newline of a whitespace run.
fn find_paragraph_break(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i < n {
        if text[i] == '\n' {
            let mut j = i + 1;
            while j < n && is_py_space(text[j]) {
                j += 1;
            }
            let mut k = j;
            while k > i {
                k -= 1;
                if k > i && text[k] == '\n' {
                    return Some((i, 0, 0, k + 1));
                }
            }
        }
        i += 1;
    }
    None
}

/// `\s*\n\s*`: a whitespace run containing at least one newline becomes one space.
fn find_soft_break(text: &[char], pos: usize) -> Option<Match> {
    let n = text.len();
    let mut i = pos;
    while i < n {
        if is_py_space(text[i]) {
            let mut j = i;
            while j < n && is_py_space(text[j]) {
                j += 1;
            }
            let mut k = j;
            while k > i {
                k -= 1;
                if text[k] == '\n' {
                    return Some((i, 0, 0, j));
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    None
}

/// Strips markdown, code, URLs, paths and emoji so the text reads as clean prose.
pub fn sanitize_for_tts(text: &str) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    chars = replace_all(&chars, find_progress, |_, _| Vec::new());
    chars = replace_all(&chars, find_fenced, |_, _| vec![' ']);
    chars = replace_all(&chars, find_unterminated_fence, |_, _| vec![' ']);
    chars = replace_all(&chars, find_inline_code, |t, m| t[m.1..m.2].to_vec());
    chars = replace_all(&chars, find_heading, |_, _| Vec::new());
    chars = replace_all(&chars, find_bold, |t, m| t[m.1..m.2].to_vec());
    chars = replace_all(
        &chars,
        |t, p| find_double_delim(t, p, '_'),
        |t, m| t[m.1..m.2].to_vec(),
    );
    chars = replace_all(
        &chars,
        |t, p| find_double_delim(t, p, '~'),
        |t, m| t[m.1..m.2].to_vec(),
    );
    chars = replace_all(&chars, find_bullet, |_, _| Vec::new());
    chars = replace_all(&chars, find_numbered, |_, _| Vec::new());
    chars = replace_all(&chars, find_link, |t, m| t[m.1..m.2].to_vec());
    chars = replace_all(&chars, find_image, |_, _| vec![' ']);
    chars = replace_all(&chars, find_quote, |_, _| Vec::new());
    chars = replace_all(&chars, find_rule, |_, _| Vec::new());
    chars = replace_all(
        &chars,
        |t, p| find_subslice(t, p, &['|']).map(|i| (i, 0, 0, i + 1)),
        |_, _| vec![',', ' '],
    );
    chars = replace_all(&chars, find_url, |_, _| "a link".chars().collect());
    chars = replace_all(&chars, find_path, |t, m| t[m.1..m.2].to_vec());
    chars.retain(|c| !is_emoji(*c));
    chars = collapse_hspace(&chars);
    chars = replace_all(&chars, find_paragraph_break, |t, m| {
        let prefix = trim_chars(&t[..m.0]);
        let replacement = match prefix.last() {
            Some('.' | '!' | '?' | ':') => " ",
            Some(_) => ". ",
            // Python's `"" in ".!?:"` is true, so an empty prefix takes the first branch.
            None => " ",
        };
        replacement.chars().collect()
    });
    chars = replace_all(&chars, find_soft_break, |_, _| vec![' ']);
    trim_chars(&chars).iter().collect()
}

// --- Sentence splitting ------------------------------------------------------

/// `[.!?]+["')\]]*(?=\s|$)`, found by `finditer`.
fn find_sentence_end(text: &[char], pos: usize) -> Option<(usize, usize)> {
    let n = text.len();
    let mut i = pos;
    while i < n {
        if matches!(text[i], '.' | '!' | '?') {
            let start = i;
            let mut end = i + 1;
            while end < n && matches!(text[end], '.' | '!' | '?') {
                end += 1;
            }
            while end < n && matches!(text[end], '"' | '\'' | ')' | ']') {
                end += 1;
            }
            if end == n || is_py_space(text[end]) {
                return Some((start, end));
            }
        }
        i += 1;
    }
    None
}

fn last_word(chars: &[char]) -> String {
    let mut end = chars.len();
    while end > 0 && is_py_space(chars[end - 1]) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && !is_py_space(chars[start - 1]) {
        start -= 1;
    }
    chars[start..end].iter().collect()
}

fn split_sentences_string(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut pos = 0;
    while let Some((punct_start, end)) = find_sentence_end(&chars, pos) {
        let word = last_word(&chars[start..=punct_start]).to_lowercase();
        if !SHORT_FORMS.contains(&word.as_str()) {
            sentences.push(trim_chars(&chars[start..end]).iter().collect());
            start = end;
        }
        pos = end;
    }
    let rest = trim_chars(&chars[start..]);
    if !rest.is_empty() {
        sentences.push(rest.iter().collect());
    }
    sentences
}

/// Splits text at sentence boundaries, leaving short forms such as `e.g.` intact.
pub fn split_sentences(text: &str) -> Vec<String> {
    split_sentences_string(text)
}

/// Returns clean prose capped to `count` sentences and `maximum` characters.
pub fn first_sentences(text: &str, count: usize, maximum: usize) -> String {
    let cleaned = sanitize_for_tts(text);
    if cleaned.is_empty() {
        return cleaned;
    }

    let sentences = split_sentences_string(&cleaned);
    let take = count.min(sentences.len());
    let joined = sentences[..take].join(" ");
    let joined_chars: Vec<char> = joined.chars().collect();
    let spoken = trim_chars(&joined_chars);
    if spoken.len() <= maximum {
        return spoken.iter().collect();
    }

    let mut chars = spoken.to_vec();
    chars.truncate(maximum);
    let cut: String = chars.into_iter().collect();
    let head = cut.rsplit_once(' ').map_or(cut.as_str(), |(h, _)| h);
    let head = head.trim_end_matches([' ', ',', ';', ':']);
    format!("{head}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_matches_python() {
        let personas = default_personas();
        assert_eq!(personas.len(), 1);
        let navi = &personas[0];
        assert_eq!(navi.name, "navi");
        assert_eq!(navi.voice, "en-GB-RyanNeural");
        assert_eq!(navi.rate, "+0%");
        assert_eq!(navi.system_prompt, NAVI_PROMPT);
        assert!(navi.system_prompt.contains("Plain prose: no markdown"));
        assert_eq!(persona_names(), vec!["navi"]);
        let alias_map = aliases();
        assert_eq!(alias_map.len(), 1);
        assert_eq!(alias_map.get("navi"), Some(&"navi"));
    }

    #[test]
    fn resolve_persona_is_case_insensitive_and_falls_back() {
        let expected = default_personas()[0].clone();
        assert_eq!(resolve_persona("navi"), expected);
        assert_eq!(resolve_persona(" NAVI "), expected);
        assert_eq!(resolve_persona("unknown"), expected);
        assert_eq!(resolve_persona(""), expected);
        assert_eq!(resolve_persona("hey navi"), expected);
    }

    #[test]
    fn persona_serializes_as_plain_data() {
        let navi = resolve_persona("navi");
        let json = serde_json::to_string(&navi).expect("persona serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json parses");
        assert_eq!(value["name"], "navi");
        assert_eq!(value["voice"], "en-GB-RyanNeural");
        assert_eq!(value["rate"], "+0%");
        let roundtrip: Persona = serde_json::from_str(&json).expect("persona deserializes");
        assert_eq!(roundtrip, navi);
    }

    #[test]
    fn sanitize_strips_markdown_code_and_bookkeeping() {
        let input = "# Heading\n\nSome **bold** and *italic* text with `code`.\n\n> quoted line\n- bullet one\n1. numbered item\n---\n| a | b |";
        assert_eq!(
            sanitize_for_tts(input),
            "Heading. Some bold and italic text with code. quoted line bullet one numbered item. , a , b ,"
        );
        assert_eq!(
            sanitize_for_tts("PROGRESS 50% done\nReal answer here."),
            "Real answer here."
        );
        assert_eq!(
            sanitize_for_tts("```rust\nlet x = 1;\n```\nAfter code. `inline` here."),
            "After code. inline here."
        );
        assert_eq!(sanitize_for_tts("Fence at end ```\nabc"), "Fence at end");
        assert_eq!(sanitize_for_tts(""), "");
    }

    #[test]
    fn sanitize_replaces_urls_paths_and_emoji() {
        assert_eq!(
            sanitize_for_tts("Check /home/user/notes.txt and src/main.rs and and/or."),
            "Check notes.txt and main.rs and and/or."
        );
        assert_eq!(
            sanitize_for_tts("See https://example.com/path?x=1 and www.foo.bar now. 😀 Done!"),
            "See a link and a link now. Done!"
        );
        assert_eq!(
            sanitize_for_tts(
                "Text with\nsoft breaks and\n\nparagraph break after dot.\n\nAnother para."
            ),
            "Text with soft breaks and. paragraph break after dot. Another para."
        );
    }

    #[test]
    fn split_sentences_keeps_short_forms_and_closers() {
        assert!(split_sentences("").is_empty());
        assert_eq!(split_sentences("x"), vec!["x"]);
        assert_eq!(
            split_sentences("Version 1.2.3 is out, e.g. on npm. Another one! And \"quoted\"."),
            vec![
                "Version 1.2.3 is out, e.g. on npm.",
                "Another one!",
                "And \"quoted\"."
            ]
        );
        assert_eq!(
            split_sentences("First sentence. Second sentence? Third one!"),
            vec!["First sentence.", "Second sentence?", "Third one!"]
        );
        assert_eq!(
            split_sentences("Hi!there is no break here."),
            vec!["Hi!there is no break here."]
        );
    }

    #[test]
    fn first_sentences_caps_sentences_and_length() {
        let text = "One short sentence. Two longer sentences here. Third final bit.";
        assert_eq!(first_sentences(text, 3, 600), text);
        assert_eq!(first_sentences(text, 1, 600), "One short sentence.");
        assert_eq!(first_sentences(text, 2, 10), "One short.");
        assert_eq!(first_sentences(text, 0, 600), "");
        assert_eq!(first_sentences(text, 3, 0), ".");
        assert_eq!(first_sentences(text, 3, 20), "One short sentence..");
        assert_eq!(first_sentences("", 3, 600), "");
    }

    #[test]
    fn text_helpers_handle_unicode_without_panicking() {
        assert_eq!(sanitize_for_tts("emoji 🚀 and VS16 ❤️"), "emoji and VS16");
        assert_eq!(split_sentences("¿Qué? Sí."), vec!["¿Qué?", "Sí."]);
        assert_eq!(first_sentences(&"á".repeat(3), 1, 2), "a\u{301}.");
    }
}
