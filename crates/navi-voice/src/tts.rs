//! TTS request building: SSML escaping, chunking, and voice/rate normalization.
//!
//! Ported from `apps/navi/workers/tts.py`. The Python module delegates the
//! actual synthesis to external providers; this module owns the deterministic
//! text preparation around those calls: resolving a voice, normalizing a rate,
//! escaping untrusted text for SSML, and splitting long text into chunks that
//! fit a provider's character budget.

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Default Edge voice used when no voice is configured (`DEFAULT_VOICE` in tts.py).
pub const DEFAULT_VOICE: &str = "en-GB-RyanNeural";

/// Edge voices selected per language when the configured voice does not match
/// the requested language (`LANGUAGE_VOICES` in tts.py).
const LANGUAGE_VOICES: &[(&str, &str)] = &[
    ("en", "en-GB-RyanNeural"),
    ("fr", "fr-FR-DeniseNeural"),
    ("de", "de-DE-KatjaNeural"),
    ("es", "es-ES-ElviraNeural"),
    ("it", "it-IT-ElsaNeural"),
    ("pt", "pt-PT-RaquelNeural"),
    ("ar", "ar-EG-SalmaNeural"),
    ("nl", "nl-NL-ColetteNeural"),
    ("pl", "pl-PL-ZofiaNeural"),
    ("ja", "ja-JP-NanamiNeural"),
    ("zh", "zh-CN-XiaoxiaoNeural"),
];

/// Abbreviations that end in a dot but do not end a sentence.
const SHORT_FORMS: &[&str] = &[
    "e.g.", "i.e.", "etc.", "vs.", "mr.", "mrs.", "ms.", "dr.", "no.", "approx.",
];

/// A prepared synthesis request. `format` is the requested audio format
/// (`"mp3"` or `"wav"`); it does not affect the SSML body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtsRequest {
    pub text: String,
    pub voice: String,
    pub rate: String,
    pub format: String,
}

impl TtsRequest {
    /// Builds a request from any string-like values.
    pub fn new(
        text: impl Into<String>,
        voice: impl Into<String>,
        rate: impl Into<String>,
        format: impl Into<String>,
    ) -> Self {
        Self {
            text: text.into(),
            voice: voice.into(),
            rate: rate.into(),
            format: format.into(),
        }
    }

    /// Returns the MIME type for the requested audio format, defaulting to MP3.
    pub fn audio_mime_type(&self) -> &'static str {
        if self.format.trim().eq_ignore_ascii_case("wav") {
            "audio/wav"
        } else {
            "audio/mpeg"
        }
    }
}

/// Escapes text for use in an XML/SSML element body or attribute value.
///
/// Ampersands are replaced first so that entities introduced by the other
/// replacements are not escaped twice.
pub fn escape_ssml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Normalizes a rate the way `_to_edge_rate` does in tts.py.
///
/// A numeric string is a speed multiplier: `1.0` becomes `+0%`, `1.25`
/// becomes `+25%`, and `0.5` becomes `-50%`. Strings that already carry a
/// percent sign (for example `+10%`) and named rates (for example `slow`) are
/// passed through unchanged; an empty rate means no change (`+0%`).
pub fn normalize_rate(rate: &str) -> String {
    let trimmed = rate.trim();
    if trimmed.is_empty() {
        return "+0%".to_owned();
    }
    match trimmed.parse::<f64>() {
        Ok(multiplier) if multiplier.is_finite() => {
            let percent = ((multiplier - 1.0) * 100.0).round() as i64;
            format!("{percent:+}%")
        }
        _ => trimmed.to_owned(),
    }
}

/// Returns the bare language code of a voice id or locale (`en-GB-RyanNeural`
/// and `en_US` both yield `en`).
pub fn language_code(value: &str) -> &str {
    match value.split_once(['-', '_']) {
        Some((language, _)) if !language.is_empty() => language,
        _ => value,
    }
}

/// Returns the language-specific default voice, or [`DEFAULT_VOICE`] when the
/// language is unknown.
pub fn default_voice_for_language(language: &str) -> &'static str {
    let code = language_code(language);
    LANGUAGE_VOICES
        .iter()
        .find(|(lang, _)| lang.eq_ignore_ascii_case(code))
        .map_or(DEFAULT_VOICE, |(_, voice)| *voice)
}

/// Resolves a voice the way `voice_for` does in tts.py: an explicit voice wins
/// when no language was requested or when it matches the requested language;
/// otherwise the language's default voice is used.
pub fn voice_for(voice: Option<&str>, language: Option<&str>) -> String {
    let requested = language.map(language_code).unwrap_or("en");
    if let Some(chosen) = voice.map(str::trim).filter(|name| !name.is_empty())
        && (language_code(chosen).eq_ignore_ascii_case(requested) || language.is_none())
    {
        return chosen.to_owned();
    }
    default_voice_for_language(requested).to_owned()
}

/// Builds the SSML body Edge TTS expects for a request.
///
/// The voice language supplies `xml:lang`, the voice name is escaped as an
/// attribute value, the rate is normalized, and the text is escaped as XML.
pub fn build_edge_ssml(req: &TtsRequest) -> String {
    let voice = resolved_voice(&req.voice);
    let language = language_tag(voice);
    let rate = normalize_rate(&req.rate);
    let text = escape_ssml(req.text.trim());
    let escaped_voice = escape_ssml(voice);
    let escaped_rate = escape_ssml(&rate);
    format!(
        "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/Synthesis\" xml:lang=\"{language}\">\
         <voice name=\"{escaped_voice}\"><prosody rate=\"{escaped_rate}\">{text}</prosody></voice></speak>"
    )
}

/// Splits text into chunks of at most `max_chars` Unicode characters.
///
/// Chunks prefer to end at a sentence boundary; long sentences are broken at
/// whitespace, and a single word longer than the budget is split at the
/// character limit. An empty text or a zero limit yields no chunks.
pub fn chunk_text_for_tts(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > max_chars {
            if current_len > 0 {
                chunks.push(std::mem::take(&mut current));
            }
            let mut rest = word;
            while rest.chars().count() > max_chars {
                let cut = rest
                    .char_indices()
                    .nth(max_chars)
                    .map_or(rest.len(), |(index, _)| index);
                chunks.push(rest[..cut].to_owned());
                rest = &rest[cut..];
            }
            current.push_str(rest);
            current_len = rest.chars().count();
            continue;
        }

        let needs_space = current_len > 0;
        if current_len + word_len + usize::from(needs_space) > max_chars {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        let needs_space = current_len > 0;
        if needs_space {
            current.push(' ');
        }
        current.push_str(word);
        current_len += word_len + usize::from(needs_space);

        if ends_sentence(word) {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
    }

    if current_len > 0 {
        chunks.push(current);
    }
    chunks
}

/// Rejects requests with no speakable text or an unsupported audio format.
pub fn validate_request(req: &TtsRequest) -> NaviResult<()> {
    if req.text.trim().is_empty() {
        return Err(NaviError::InvalidInput(
            "TTS text must not be empty".to_owned(),
        ));
    }
    let format = req.format.trim().to_ascii_lowercase();
    if format != "mp3" && format != "wav" {
        return Err(NaviError::InvalidInput(format!(
            "unsupported TTS audio format '{}'",
            req.format
        )));
    }
    Ok(())
}

fn resolved_voice(voice: &str) -> &str {
    let trimmed = voice.trim();
    if trimmed.is_empty() {
        DEFAULT_VOICE
    } else {
        trimmed
    }
}

fn language_tag(voice: &str) -> String {
    let mut parts = voice.split(['-', '_']);
    let language = match parts.next() {
        Some(value) if !value.is_empty() => value,
        _ => "en",
    };
    match parts.next() {
        Some(region)
            if region.len() == 2 && region.bytes().all(|byte| byte.is_ascii_alphabetic()) =>
        {
            format!("{language}-{}", region.to_ascii_uppercase())
        }
        _ => language.to_owned(),
    }
}

fn ends_sentence(word: &str) -> bool {
    let trimmed = word.trim_end_matches(['"', '\'', ')', ']', '”', '’']);
    if SHORT_FORMS.contains(&trimmed.to_ascii_lowercase().as_str()) {
        return false;
    }
    matches!(
        trimmed.chars().last(),
        Some('.' | '!' | '?' | '。' | '！' | '？')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_ssml_replaces_xml_metacharacters_once() {
        let escaped = escape_ssml("Tom & \"Jerry\" <dog> > cat's &lt;");
        assert_eq!(
            escaped,
            "Tom &amp; &quot;Jerry&quot; &lt;dog&gt; &gt; cat&apos;s &amp;lt;"
        );
    }

    #[test]
    fn escape_ssml_keeps_plain_and_unicode_text_intact() {
        let text = "Hello, world — مرحبا 世界";
        assert_eq!(escape_ssml(text), text);
        assert_eq!(escape_ssml(""), "");
    }

    #[test]
    fn normalize_rate_handles_multipliers_percent_strings_and_empty_input() {
        assert_eq!(normalize_rate(""), "+0%");
        assert_eq!(normalize_rate("   "), "+0%");
        assert_eq!(normalize_rate("1"), "+0%");
        assert_eq!(normalize_rate("1.25"), "+25%");
        assert_eq!(normalize_rate("2"), "+100%");
        assert_eq!(normalize_rate("0.5"), "-50%");
        assert_eq!(normalize_rate("+10%"), "+10%");
        assert_eq!(normalize_rate("10%"), "10%");
        assert_eq!(normalize_rate("slow"), "slow");
    }

    #[test]
    fn build_edge_ssml_uses_defaults_and_escapes_untrusted_text() {
        let req = TtsRequest::new("Hello <world> & friends", "", "", "mp3");
        assert_eq!(
            build_edge_ssml(&req),
            "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/Synthesis\" \
             xml:lang=\"en-GB\"><voice name=\"en-GB-RyanNeural\">\
             <prosody rate=\"+0%\">Hello &lt;world&gt; &amp; friends</prosody></voice></speak>"
        );
    }

    #[test]
    fn build_edge_ssml_honors_voice_language_and_normalized_rate() {
        let req = TtsRequest::new("Bonjour.", "fr-FR-DeniseNeural", "1.25", "wav");
        let ssml = build_edge_ssml(&req);
        assert!(ssml.contains("xml:lang=\"fr-FR\""));
        assert!(ssml.contains("name=\"fr-FR-DeniseNeural\""));
        assert!(ssml.contains("rate=\"+25%\""));
        assert!(ssml.contains(">Bonjour.</prosody>"));
        assert_eq!(req.audio_mime_type(), "audio/wav");
    }

    #[test]
    fn voice_for_matches_python_resolution_rules() {
        assert_eq!(
            voice_for(Some("en-US-JennyNeural"), None),
            "en-US-JennyNeural"
        );
        assert_eq!(
            voice_for(Some("en-US-JennyNeural"), Some("fr")),
            "fr-FR-DeniseNeural"
        );
        assert_eq!(voice_for(None, Some("ja")), "ja-JP-NanamiNeural");
        assert_eq!(voice_for(None, Some("zz")), DEFAULT_VOICE);
        assert_eq!(voice_for(None, None), DEFAULT_VOICE);
        assert_eq!(default_voice_for_language("de-DE"), "de-DE-KatjaNeural");
    }

    #[test]
    fn chunk_text_for_tts_splits_on_sentence_boundaries() {
        let chunks = chunk_text_for_tts("First sentence. Second sentence! Third one?", 40);
        assert_eq!(
            chunks,
            ["First sentence.", "Second sentence!", "Third one?"]
        );
    }

    #[test]
    fn chunk_text_for_tts_splits_long_sentences_on_words() {
        let text = "This is a very long sentence that must be split across chunks.";
        let chunks = chunk_text_for_tts(text, 20);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 20));
        let joined: String = chunks.concat();
        assert_eq!(
            joined
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>(),
            text.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        );
    }

    #[test]
    fn chunk_text_for_tts_hard_splits_words_and_counts_unicode_chars() {
        assert_eq!(chunk_text_for_tts("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(chunk_text_for_tts("héllo wörld", 7), ["héllo", "wörld"]);
    }

    #[test]
    fn chunk_text_for_tts_handles_empty_text_and_zero_limit() {
        assert!(chunk_text_for_tts("", 100).is_empty());
        assert!(chunk_text_for_tts("  \n\t ", 100).is_empty());
        assert!(chunk_text_for_tts("hello world", 0).is_empty());
    }

    #[test]
    fn validate_request_rejects_empty_text_and_unknown_formats() {
        let good = TtsRequest::new("hello", "en-GB-RyanNeural", "+0%", "MP3");
        assert!(validate_request(&good).is_ok());
        assert!(validate_request(&TtsRequest::new("  ", "", "", "mp3")).is_err());
        assert!(validate_request(&TtsRequest::new("hello", "", "", "ogg")).is_err());
    }

    #[test]
    fn request_roundtrips_through_json() {
        let req = TtsRequest::new("hello", "en-GB-RyanNeural", "+0%", "mp3");
        let json = serde_json::to_string(&req).expect("serialize request");
        assert!(json.contains("\"format\":\"mp3\""));
        let back: TtsRequest = serde_json::from_str(&json).expect("deserialize request");
        assert_eq!(back, req);
    }
}
