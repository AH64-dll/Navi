//! Speech-to-text helpers ported from `cloud_stt.py` and `hex_asr.py`.
//!
//! The module owns the pieces that do not need a network client: PCM16 WAV
//! encoding, HTTP request bodies (multipart for Groq/OpenAI-style endpoints
//! and JSON for Gemini), provider response parsing, confidence filtering and
//! a per-provider circuit breaker.

use navi_types::{NaviError, NaviResult};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A normalized transcript returned by one STT provider.
#[derive(Debug, Clone, PartialEq)]
pub struct SttResult {
    /// Recognized speech, verbatim from the provider.
    pub text: String,
    /// Provider-reported confidence, when the provider reports one.
    pub confidence: Option<f64>,
    /// BCP-47 language tag, when present in the provider response.
    pub language: Option<String>,
    /// Name of the provider that produced the transcript.
    pub provider: String,
}

impl SttResult {
    /// Creates a result, leaving confidence and language unset.
    pub fn new(provider: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            confidence: None,
            language: None,
            provider: provider.into(),
        }
    }
}

/// RIFF/WAV header for 16-bit PCM data.
///
/// `data_len` is the size of the sample data in bytes. Values that cannot be
/// represented in a 32-bit RIFF chunk are saturated rather than wrapped.
fn wav_header(data_len: usize, sample_rate: u32, channels: u16) -> [u8; 44] {
    let channels = channels.max(1);
    let data_len = u32::try_from(data_len).unwrap_or(u32::MAX);
    let byte_rate = (sample_rate as u64)
        .saturating_mul(u64::from(channels))
        .saturating_mul(2)
        .min(u64::from(u32::MAX)) as u32;
    let block_align = channels.saturating_mul(2);

    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_len.to_le_bytes());
    header
}

/// Encodes interleaved PCM16 samples as a complete little-endian RIFF WAV.
///
/// An empty sample slice produces a valid 44-byte header with no data chunk
/// payload, matching `wave.writeframes(b"")`.
pub fn encode_wav_pcm16(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = samples.len().saturating_mul(2);
    let mut wav = Vec::with_capacity(44usize.saturating_add(data_len));
    wav.extend_from_slice(&wav_header(data_len, sample_rate, channels));
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// Returns true when `bytes` already starts with a RIFF/WAVE header.
pub fn is_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// Passes a complete WAV through and wraps raw little-endian PCM16 otherwise.
///
/// Rejects an odd number of raw bytes, which cannot form a PCM16 frame.
pub fn pcm16_to_wav(audio: &[u8], sample_rate: u32, channels: u16) -> NaviResult<Vec<u8>> {
    if is_wav(audio) {
        return Ok(audio.to_vec());
    }
    if !audio.len().is_multiple_of(2) {
        return Err(NaviError::InvalidInput(
            "raw PCM16 audio must contain an even number of bytes".to_owned(),
        ));
    }
    let mut wav = Vec::with_capacity(44usize.saturating_add(audio.len()));
    wav.extend_from_slice(&wav_header(audio.len(), sample_rate, channels));
    wav.extend_from_slice(audio);
    Ok(wav)
}

/// Converts a JSON confidence value (number or numeric string) to `f64`.
///
/// Non-finite values are rejected so they cannot poison later comparisons.
fn confidence_from_value(value: &Value) -> Option<f64> {
    let confidence = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    confidence.filter(|value| value.is_finite())
}

/// Reads an optional string field from an optional JSON object.
fn text_field(object: Option<&Value>, key: &str) -> Option<String> {
    object
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Reads an optional confidence field from an optional JSON object.
fn confidence_field(object: Option<&Value>) -> Option<f64> {
    object
        .and_then(|value| value.get("confidence"))
        .and_then(confidence_from_value)
}

/// Reads a language tag, accepting both `language` and Gemini's
/// `language_codes` array.
fn language_field(object: Option<&Value>) -> Option<String> {
    let object = object?;
    if let Some(language) = object
        .get("language")
        .and_then(Value::as_str)
        .filter(|language| !language.trim().is_empty())
    {
        return Some(language.to_owned());
    }
    object
        .get("language_codes")
        .and_then(Value::as_array)
        .and_then(|codes| codes.iter().find_map(Value::as_str))
        .filter(|language| !language.trim().is_empty())
        .map(str::to_owned)
}

/// Extracts the first text step from a Gemini interactions response.
fn gemini_text(value: &Value) -> (Option<String>, Option<f64>, Option<String>) {
    let language = language_field(Some(value));
    for step in value
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for content in step
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if content.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = content
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            let confidence = content.get("confidence").and_then(confidence_from_value);
            return (Some(text.to_owned()), confidence, language);
        }
    }
    (None, None, language)
}

/// Parses a whisper-style JSON response (Groq, Cloudflare, Gemini).
///
/// The `provider` argument selects the response shape and is copied into the
/// returned [`SttResult`]. Cloudflare responses with `"success": false` are
/// reported as provider errors.
pub fn parse_whisper_response(body: &str, provider: &str) -> NaviResult<SttResult> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        NaviError::Protocol(format!("invalid {provider} STT response: {error}"))
    })?;
    if !value.is_object() {
        return Err(NaviError::Protocol(format!(
            "{provider} STT response must be a JSON object"
        )));
    }

    let (text, confidence, language) = match provider.trim().to_ascii_lowercase().as_str() {
        "cloudflare" => {
            if value.get("success").and_then(Value::as_bool) == Some(false) {
                return Err(NaviError::Provider(
                    "cloudflare whisper reported failure".to_owned(),
                ));
            }
            let result = value.get("result");
            (
                text_field(result, "text"),
                confidence_field(result),
                language_field(result).or_else(|| language_field(Some(&value))),
            )
        }
        "gemini" => gemini_text(&value),
        _ => (
            text_field(Some(&value), "text"),
            confidence_field(Some(&value)),
            language_field(Some(&value)),
        ),
    };

    Ok(SttResult {
        text: text.unwrap_or_default(),
        confidence,
        language,
        provider: provider.to_owned(),
    })
}

/// Parses a Deepgram `/v1/listen` JSON response.
///
/// Uses the first alternative of the first channel, as the Python chain does.
pub fn parse_deepgram_response(body: &str) -> NaviResult<SttResult> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| NaviError::Protocol(format!("invalid deepgram STT response: {error}")))?;
    if !value.is_object() {
        return Err(NaviError::Protocol(
            "deepgram STT response must be a JSON object".to_owned(),
        ));
    }

    let results = value.get("results");
    let channel = results
        .and_then(|results| results.get("channels"))
        .and_then(Value::as_array)
        .and_then(|channels| channels.first());
    let alternative = channel
        .and_then(|channel| channel.get("alternatives"))
        .and_then(Value::as_array)
        .and_then(|alternatives| alternatives.first());

    Ok(SttResult {
        text: text_field(alternative, "transcript").unwrap_or_default(),
        confidence: confidence_field(alternative),
        language: language_field(results)
            .or_else(|| language_field(channel))
            .or_else(|| language_field(Some(&value))),
        provider: "deepgram".to_owned(),
    })
}

/// Escapes characters that would break a multipart header value.
fn sanitize_header_value(value: &str) -> String {
    value
        .replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Creates a reasonably unique multipart boundary without pulling in a UUID
/// dependency.
pub fn generate_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("navi-stt-{nanos:x}-{counter:x}")
}

/// Builds a `multipart/form-data` body and its `Content-Type` value.
///
/// Fields are emitted in iteration order followed by the WAV file part, which
/// mirrors `_multipart()` in `cloud_stt.py`.
pub fn build_multipart_body<K: AsRef<str>, V: AsRef<str>>(
    fields: &[(K, V)],
    file_name: &str,
    file_bytes: &[u8],
    content_type: &str,
    boundary: &str,
) -> (Vec<u8>, String) {
    let mut body = Vec::new();
    for (key, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n",
                sanitize_header_value(key.as_ref())
            )
            .as_bytes(),
        );
        body.extend_from_slice(value.as_ref().as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: {content_type}\r\n\r\n",
            sanitize_header_value(file_name)
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (body, format!("multipart/form-data; boundary={boundary}"))
}

/// Builds the form fields used by Groq/OpenAI-style transcription endpoints.
///
/// Keywords become a `prompt` hint, capped at 20 entries as in the Python
/// implementation.
pub fn whisper_form_fields(
    model: &str,
    response_format: &str,
    language: Option<&str>,
    keywords: &[String],
) -> Vec<(String, String)> {
    let mut fields = vec![
        ("model".to_owned(), model.to_owned()),
        ("response_format".to_owned(), response_format.to_owned()),
        ("temperature".to_owned(), "0".to_owned()),
    ];
    if let Some(language) = language.filter(|language| !language.trim().is_empty()) {
        fields.push(("language".to_owned(), language.to_owned()));
    }
    if !keywords.is_empty() {
        let vocabulary = keywords
            .iter()
            .take(20)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(("prompt".to_owned(), format!("Vocabulary: {vocabulary}")));
    }
    fields
}

/// Percent-encodes a query-string value using RFC 3986 unreserved characters.
pub fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

/// Appends Deepgram language and hotword query parameters to a base URL.
pub fn build_deepgram_url(base: &str, language: Option<&str>, keywords: &[String]) -> String {
    let mut url = base.to_owned();
    if let Some(language) = language.filter(|language| !language.trim().is_empty()) {
        url.push_str("&language=");
        url.push_str(&percent_encode(language));
    }
    for keyword in keywords.iter().take(20) {
        url.push_str("&keywords=");
        url.push_str(&percent_encode(keyword));
    }
    url
}

/// Standard base64 encoder, used for Gemini's inline audio payload.
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[((triple >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((triple >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

/// Builds the JSON request body for Gemini's transcription interaction.
pub fn build_gemini_payload(
    wav: &[u8],
    language: Option<&str>,
    keywords: &[String],
    model: &str,
) -> NaviResult<Vec<u8>> {
    let language_codes: Vec<String> = language
        .filter(|language| !language.trim().is_empty())
        .map(str::to_owned)
        .into_iter()
        .collect();
    let hints: Vec<String> = keywords.iter().take(20).cloned().collect();
    let payload = json!({
        "model": model,
        "input": [{
            "type": "audio",
            "mime_type": "audio/wav",
            "data": base64_encode(wav),
        }],
        "generation_config": {
            "transcription_config": {
                "mode": "smart",
                "language_codes": language_codes,
                "hints": hints,
            },
        },
    });
    Ok(serde_json::to_vec(&payload)?)
}

/// Returns true when a result has no confidence score or meets the floor.
///
/// Providers that do not report confidence are kept: dropping them would make
/// every Groq or Cloudflare transcript unusable.
pub fn is_confident(result: &SttResult, min_confidence: f64) -> bool {
    result
        .confidence
        .is_none_or(|confidence| confidence >= min_confidence)
}

/// Keeps only results whose confidence meets the given floor.
pub fn filter_by_confidence(results: Vec<SttResult>, min_confidence: f64) -> Vec<SttResult> {
    results
        .into_iter()
        .filter(|result| is_confident(result, min_confidence))
        .collect()
}

/// Circuit breaker for one cloud STT provider.
///
/// Consecutive failures open the breaker after `failure_threshold` trips;
/// HTTP 401/403 failures open it immediately. A success clears all state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreaker {
    failures: u32,
    failure_threshold: u32,
    cooldown: Duration,
    open_until: Option<Instant>,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(2, Duration::from_secs(30))
    }
}

impl CircuitBreaker {
    /// Creates a closed breaker with the given threshold and open duration.
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            failures: 0,
            failure_threshold: failure_threshold.max(1),
            cooldown,
            open_until: None,
        }
    }

    /// Returns true if the breaker is currently open.
    pub fn is_open(&self) -> bool {
        self.is_open_at(Instant::now())
    }

    /// Returns true if the breaker is open at the supplied instant.
    pub fn is_open_at(&self, now: Instant) -> bool {
        self.open_until.is_some_and(|until| now < until)
    }

    /// Number of consecutive failures counted since the last success.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Instant at which the breaker closes again, if it is tripped.
    pub fn open_until(&self) -> Option<Instant> {
        self.open_until
    }

    /// Remaining time before the breaker closes again.
    pub fn retry_after(&self, now: Instant) -> Option<Duration> {
        self.open_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    /// Records an ordinary (transient or permanent) provider failure.
    pub fn record_failure(&mut self) {
        self.record_failure_at(Instant::now());
    }

    /// Records an ordinary failure at the supplied instant.
    pub fn record_failure_at(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.failure_threshold {
            self.trip(now);
        }
    }

    /// Records an authentication failure (HTTP 401/403), opening immediately.
    pub fn record_auth_failure(&mut self) {
        self.record_auth_failure_at(Instant::now());
    }

    /// Records an authentication failure at the supplied instant.
    pub fn record_auth_failure_at(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.trip(now);
    }

    /// Records a successful call and clears failure history.
    pub fn record_success(&mut self) {
        self.reset();
    }

    /// Returns the breaker to its closed, pristine state.
    pub fn reset(&mut self) {
        self.failures = 0;
        self.open_until = None;
    }

    fn trip(&mut self, now: Instant) {
        self.open_until = Some(now + self.cooldown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_wav_pcm16_writes_riff_header_and_little_endian_samples() {
        let wav = encode_wav_pcm16(&[0, 1, -1, 32767], 16_000, 1);
        assert_eq!(wav.len(), 44 + 8);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[4..8], &44u32.to_le_bytes());
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[16..20], &16u32.to_le_bytes());
        assert_eq!(&wav[20..22], &1u16.to_le_bytes());
        assert_eq!(&wav[22..24], &1u16.to_le_bytes());
        assert_eq!(&wav[24..28], &16_000u32.to_le_bytes());
        assert_eq!(&wav[28..32], &32_000u32.to_le_bytes());
        assert_eq!(&wav[32..34], &2u16.to_le_bytes());
        assert_eq!(&wav[34..36], &16u16.to_le_bytes());
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(&wav[40..44], &8u32.to_le_bytes());
        assert_eq!(&wav[44..], &[0u8, 0, 1, 0, 255, 255, 255, 127]);
        assert!(is_wav(&wav));
    }

    #[test]
    fn encode_wav_pcm16_handles_empty_and_stereo_audio() {
        let empty = encode_wav_pcm16(&[], 8_000, 1);
        assert_eq!(empty.len(), 44);
        assert_eq!(&empty[40..44], &0u32.to_le_bytes());

        let stereo = encode_wav_pcm16(&[10, 20], 44_100, 2);
        assert_eq!(&stereo[22..24], &2u16.to_le_bytes());
        assert_eq!(&stereo[28..32], &176_400u32.to_le_bytes());
        assert_eq!(&stereo[32..34], &4u16.to_le_bytes());
        assert_eq!(&stereo[40..44], &4u32.to_le_bytes());

        let no_channels = encode_wav_pcm16(&[1], 8_000, 0);
        assert_eq!(&no_channels[22..24], &1u16.to_le_bytes());
    }

    #[test]
    fn pcm16_to_wav_passes_through_wav_and_wraps_raw_samples() {
        let wav = encode_wav_pcm16(&[1, 2], 16_000, 1);
        assert_eq!(pcm16_to_wav(&wav, 8_000, 1).expect("passthrough"), wav);

        let raw = [1u8, 0, 2, 0];
        let wrapped = pcm16_to_wav(&raw, 16_000, 1).expect("wrapped");
        assert_eq!(&wrapped[..44], &encode_wav_pcm16(&[1, 2], 16_000, 1)[..44]);
        assert_eq!(&wrapped[44..], &raw);

        assert!(pcm16_to_wav(&[1u8, 2, 3], 16_000, 1).is_err());
    }

    #[test]
    fn parse_whisper_response_handles_groq_cloudflare_and_gemini() {
        let groq = parse_whisper_response(r#"{"text":"hello world"}"#, "groq").expect("groq");
        assert_eq!(groq.text, "hello world");
        assert_eq!(groq.provider, "groq");
        assert_eq!(groq.confidence, None);
        assert_eq!(groq.language, None);

        let cloudflare = parse_whisper_response(
            r#"{"success":true,"result":{"text":"from the edge"}}"#,
            "cloudflare",
        )
        .expect("cloudflare");
        assert_eq!(cloudflare.text, "from the edge");
        assert_eq!(cloudflare.provider, "cloudflare");

        let gemini = parse_whisper_response(
            r#"{"steps":[{"content":[{"type":"text","text":"hi","confidence":0.8}]}]}"#,
            "gemini",
        )
        .expect("gemini");
        assert_eq!(gemini.text, "hi");
        assert_eq!(gemini.confidence, Some(0.8));

        let empty = parse_whisper_response("{}", "groq").expect("empty");
        assert_eq!(empty.text, "");
    }

    #[test]
    fn parse_whisper_response_rejects_failures_and_malformed_json() {
        let failure = parse_whisper_response(r#"{"success":false}"#, "cloudflare");
        assert!(matches!(failure, Err(NaviError::Provider(_))));
        assert!(parse_whisper_response("not json", "groq").is_err());
        assert!(parse_whisper_response("[]", "groq").is_err());
    }

    #[test]
    fn parse_deepgram_response_reads_first_alternative_and_language() {
        let body = r#"{
            "metadata": {"request_id": "x"},
            "results": {
                "language": "en",
                "channels": [{
                    "alternatives": [
                        {"transcript": "hello there", "confidence": 0.93},
                        {"transcript": "second", "confidence": 0.5}
                    ]
                }]
            }
        }"#;
        let result = parse_deepgram_response(body).expect("deepgram");
        assert_eq!(result.text, "hello there");
        assert_eq!(result.confidence, Some(0.93));
        assert_eq!(result.language.as_deref(), Some("en"));
        assert_eq!(result.provider, "deepgram");
    }

    #[test]
    fn parse_deepgram_response_handles_missing_and_odd_fields() {
        let empty = parse_deepgram_response("{}").expect("empty");
        assert_eq!(empty.text, "");
        assert_eq!(empty.confidence, None);

        let string_confidence = parse_deepgram_response(
            r#"{"results":{"channels":[{"alternatives":[{"transcript":"x","confidence":"0.75"}]}]}}"#,
        )
        .expect("string confidence");
        assert_eq!(string_confidence.confidence, Some(0.75));

        let nan = parse_deepgram_response(
            r#"{"results":{"channels":[{"alternatives":[{"transcript":"x","confidence":"NaN"}]}]}}"#,
        )
        .expect("NaN");
        assert_eq!(nan.confidence, None);

        assert!(parse_deepgram_response("{").is_err());
    }

    #[test]
    fn circuit_breaker_trips_after_threshold_and_clears_on_success() {
        let now = Instant::now();
        let mut breaker = CircuitBreaker::new(2, Duration::from_secs(30));
        assert!(!breaker.is_open_at(now));
        assert_eq!(breaker.failures(), 0);

        breaker.record_failure_at(now);
        assert!(!breaker.is_open_at(now));
        assert_eq!(breaker.failures(), 1);

        breaker.record_failure_at(now);
        assert!(breaker.is_open_at(now));
        assert_eq!(breaker.retry_after(now), Some(Duration::from_secs(30)));
        assert!(!breaker.is_open_at(now + Duration::from_secs(31)));
        assert!(breaker.retry_after(now + Duration::from_secs(31)).is_none());

        breaker.record_success();
        assert!(!breaker.is_open_at(now));
        assert_eq!(breaker.failures(), 0);
        assert_eq!(breaker.open_until(), None);
    }

    #[test]
    fn circuit_breaker_auth_failure_opens_immediately() {
        let now = Instant::now();
        let mut breaker = CircuitBreaker::default();
        breaker.record_auth_failure_at(now);
        assert!(breaker.is_open_at(now));
        assert_eq!(breaker.failures(), 1);

        breaker.reset();
        assert!(!breaker.is_open_at(now));
        breaker.record_failure_at(now);
        breaker.record_failure_at(now);
        breaker.record_failure_at(now);
        assert!(breaker.is_open_at(now));
    }

    #[test]
    fn multipart_body_contains_fields_wav_part_and_content_type() {
        let boundary = "test-boundary";
        let fields = [("model", "whisper-large-v3"), ("temperature", "0")];
        let (body, content_type) =
            build_multipart_body(&fields, "a.wav", &[1, 2, 3], "audio/wav", boundary);
        let body = String::from_utf8(body).expect("utf8 body");
        assert_eq!(content_type, "multipart/form-data; boundary=test-boundary");
        assert!(body.starts_with("--test-boundary\r\n"));
        assert!(body.contains("name=\"model\"\r\n\r\nwhisper-large-v3\r\n"));
        assert!(body.contains("name=\"file\"; filename=\"a.wav\"\r\n"));
        assert!(body.contains("Content-Type: audio/wav\r\n\r\n"));
        assert!(body.ends_with("\r\n--test-boundary--\r\n"));
        assert!(!generate_boundary().is_empty());
    }

    #[test]
    fn request_builders_and_confidence_filter_behave() {
        let keywords = vec!["Navi".to_owned(), "a b".to_owned()];
        let fields = whisper_form_fields("whisper-large-v3", "json", Some("en"), &keywords);
        assert_eq!(fields[3].0, "language");
        assert_eq!(fields[4].1, "Vocabulary: Navi, a b");

        let url = build_deepgram_url(
            "https://api.deepgram.com/v1/listen",
            Some("en-US"),
            &keywords,
        );
        assert_eq!(
            url,
            "https://api.deepgram.com/v1/listen&language=en-US&keywords=Navi&keywords=a%20b"
        );

        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");

        let payload = build_gemini_payload(&[1, 2, 3], Some("en"), &keywords, "model-x")
            .expect("gemini payload");
        let parsed: Value = serde_json::from_slice(&payload).expect("payload json");
        assert_eq!(parsed["model"], "model-x");
        assert_eq!(parsed["input"][0]["data"], "AQID");
        assert_eq!(
            parsed["generation_config"]["transcription_config"]["hints"][1],
            "a b"
        );

        let mut high = SttResult::new("groq", "confident");
        high.confidence = Some(0.9);
        let mut low = SttResult::new("groq", "unsure");
        low.confidence = Some(0.2);
        let unknown = SttResult::new("cloudflare", "unknown");
        assert!(is_confident(&high, 0.5));
        assert!(!is_confident(&low, 0.5));
        assert!(is_confident(&unknown, 0.5));
        let filtered = filter_by_confidence(vec![high.clone(), low, unknown.clone()], 0.5);
        assert_eq!(filtered, vec![high, unknown]);
    }
}
