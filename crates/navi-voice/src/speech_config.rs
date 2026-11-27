//! Speech provider configuration ported from `apps/navi/workers/speech_config.py`.
//!
//! The Python module keeps two module-level caches (`speech()` and `secrets()`).
//! This port keeps the same data as plain values instead: [`load_speech_config`]
//! reads a JSON file, merges it over the documented defaults, and normalises an
//! invalid `speech_mode` back to the default. [`SpeechSecrets`] reads credentials
//! from the process environment first and from a `KEY=VALUE` file second; its
//! `Debug` output never reveals a secret value.

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

/// Credential names read by the Python module for cloud providers.
pub const KEY_NAMES: [&str; 5] = [
    "DEEPGRAM_API_KEY",
    "GROQ_API_KEY",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_API_TOKEN",
    "GEMINI_API_KEY",
];

/// Cloud text-to-speech providers, in the default priority order.
pub const CLOUD_TTS: [&str; 3] = ["edge", "cloudflare", "gemini"];
/// Local text-to-speech providers, in the default priority order.
pub const LOCAL_TTS: [&str; 1] = ["piper"];
/// Cloud speech-to-text providers, in the default priority order.
pub const CLOUD_STT: [&str; 4] = ["deepgram", "groq", "cloudflare", "gemini"];
/// Local speech-to-text providers, in the default priority order.
pub const LOCAL_STT: [&str; 1] = ["sherpa"];

const DEFAULT_TIMEOUT_SECONDS: f64 = 3.0;
const DEFAULT_BREAKER_SECONDS: f64 = 300.0;
const DEFAULT_BREAKER_FAILURES: i64 = 2;
const DEFAULT_RETRY_ATTEMPTS: i64 = 1;
const DEFAULT_TTS_VOICE: &str = "en-GB-RyanNeural";
const DEFAULT_MIN_CONFIDENCE: f64 = 0.6;
const DEFAULT_CONFIRM_SECONDS: f64 = 1.5;

/// Provider selection policy: cloud hops first, local hops only, or cloud hops only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeechMode {
    /// Cloud providers are attempted before local providers.
    #[default]
    CloudFirst,
    /// Every cloud provider is skipped.
    LocalOnly,
    /// Every local provider is skipped.
    CloudOnly,
}

impl SpeechMode {
    /// Returns the spelling used by `speech.json` and the Python module.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CloudFirst => "cloud-first",
            Self::LocalOnly => "local-only",
            Self::CloudOnly => "cloud-only",
        }
    }
}

impl fmt::Display for SpeechMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed equivalent of the Python `DEFAULTS` dict merged with `speech.json`.
///
/// Numeric fields keep the value parsed from the file; the clamping described in
/// the Python accessors is applied by the accessor methods and free functions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpeechConfig {
    pub speech_mode: SpeechMode,
    pub tts_order: Vec<String>,
    pub stt_order: Vec<String>,
    pub disabled: Vec<String>,
    pub timeout_seconds: f64,
    pub breaker_seconds: f64,
    pub breaker_failures: i64,
    pub retry_attempts: i64,
    pub tts_voice: String,
    pub language: String,
    pub min_confidence: f64,
    pub confirm_seconds: f64,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            speech_mode: SpeechMode::default(),
            tts_order: default_order(&CLOUD_TTS, &LOCAL_TTS),
            // Python's DEFAULTS sets "stt_order": list(CLOUD_STT) only; the local
            // STT provider (sherpa) is never part of the default order.
            stt_order: default_order(&CLOUD_STT, &[]),
            disabled: Vec::new(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            breaker_seconds: DEFAULT_BREAKER_SECONDS,
            breaker_failures: DEFAULT_BREAKER_FAILURES,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            tts_voice: DEFAULT_TTS_VOICE.to_string(),
            language: String::new(),
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            confirm_seconds: DEFAULT_CONFIRM_SECONDS,
        }
    }
}

impl SpeechConfig {
    /// Merges a decoded JSON value over the defaults, tolerating wrong types.
    ///
    /// Coercions follow the Python source: `int()`/`float()` for numbers,
    /// `list()`/`set()` for provider lists, and a fallback to the default for an
    /// unrecognised `speech_mode`.
    pub fn from_value(value: &Value) -> Self {
        let Some(obj) = value.as_object() else {
            return Self::default();
        };
        let get = |key: &str| obj.get(key);

        let speech_mode = get("speech_mode")
            .and_then(Value::as_str)
            .and_then(parse_mode)
            .unwrap_or_default();
        let tts_order = match get("tts_order") {
            Some(value) => py_string_list(value),
            None => default_order(&CLOUD_TTS, &LOCAL_TTS),
        };
        let stt_order = match get("stt_order") {
            Some(value) => py_string_list(value),
            None => default_order(&CLOUD_STT, &[]),
        };
        let disabled = get("disabled")
            .map(py_string_list)
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        Self {
            speech_mode,
            tts_order,
            stt_order,
            disabled,
            timeout_seconds: get("timeout_seconds").map_or(DEFAULT_TIMEOUT_SECONDS, |v| {
                py_float(v, DEFAULT_TIMEOUT_SECONDS)
            }),
            breaker_seconds: get("breaker_seconds").map_or(DEFAULT_BREAKER_SECONDS, |v| {
                py_float(v, DEFAULT_BREAKER_SECONDS)
            }),
            breaker_failures: get("breaker_failures").map_or(DEFAULT_BREAKER_FAILURES, |v| {
                py_int(v, DEFAULT_BREAKER_FAILURES)
            }),
            retry_attempts: get("retry_attempts").map_or(DEFAULT_RETRY_ATTEMPTS, |v| {
                py_int(v, DEFAULT_RETRY_ATTEMPTS)
            }),
            tts_voice: get("tts_voice").map_or_else(|| DEFAULT_TTS_VOICE.to_string(), py_str),
            language: match get("language") {
                Some(value) if py_truthy(value) => py_str(value),
                _ => String::new(),
            },
            min_confidence: get("min_confidence").map_or(DEFAULT_MIN_CONFIDENCE, |v| {
                py_float(v, DEFAULT_MIN_CONFIDENCE)
            }),
            confirm_seconds: get("confirm_seconds").map_or(DEFAULT_CONFIRM_SECONDS, |v| {
                py_float(v, DEFAULT_CONFIRM_SECONDS)
            }),
        }
    }

    /// Returns the configured provider selection mode.
    pub fn speech_mode(&self) -> SpeechMode {
        self.speech_mode
    }

    /// Returns the request timeout in seconds.
    pub fn timeout_seconds(&self) -> f64 {
        self.timeout_seconds
    }

    /// Returns the circuit-breaker open duration in seconds.
    pub fn breaker_seconds(&self) -> f64 {
        self.breaker_seconds
    }

    /// Returns consecutive failures before a breaker opens, at least one.
    pub fn breaker_failures(&self) -> i64 {
        self.breaker_failures.max(1)
    }

    /// Returns extra retries per provider, never negative.
    pub fn retry_attempts(&self) -> i64 {
        self.retry_attempts.max(0)
    }

    /// Returns the confidence threshold below which a transcript is confirmed.
    pub fn min_confidence(&self) -> f64 {
        self.min_confidence
    }

    /// Returns the confirmation window in seconds, never negative.
    pub fn confirm_seconds(&self) -> f64 {
        self.confirm_seconds.max(0.0)
    }

    /// Returns the configured language, or a language derived from the locale.
    pub fn language(&self) -> String {
        language(self)
    }

    /// Returns the provider order for `"tts"` or `"stt"`.
    pub fn provider_order(&self, env_keys: Option<&SpeechSecrets>, kind: &str) -> Vec<String> {
        provider_order(self, env_keys, kind)
    }
}

impl<'de> Deserialize<'de> for SpeechConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(&Value::deserialize(deserializer)?))
    }
}

/// Reads speech tuning from `path`, using the defaults when the file is absent.
///
/// Invalid JSON and a non-object document are ignored, matching the Python
/// `_load_speech_file` helper. Other read failures are reported to the caller.
pub fn load_speech_config(path: &Path) -> NaviResult<SpeechConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(SpeechConfig::from_value(&parse_config_json(&text))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(SpeechConfig::default()),
        Err(err) => Err(NaviError::Io(format!("{}: {err}", path.display()))),
    }
}

/// Returns the configured provider selection mode.
pub fn speech_mode(cfg: &SpeechConfig) -> SpeechMode {
    cfg.speech_mode()
}

/// Returns the request timeout in seconds.
pub fn timeout_seconds(cfg: &SpeechConfig) -> f64 {
    cfg.timeout_seconds()
}

/// Returns the circuit-breaker open duration in seconds.
pub fn breaker_seconds(cfg: &SpeechConfig) -> f64 {
    cfg.breaker_seconds()
}

/// Returns consecutive failures before a breaker opens, at least one.
pub fn breaker_failures(cfg: &SpeechConfig) -> i64 {
    cfg.breaker_failures()
}

/// Returns extra retries per provider, never negative.
pub fn retry_attempts(cfg: &SpeechConfig) -> i64 {
    cfg.retry_attempts()
}

/// Returns the confidence threshold below which a transcript is confirmed.
pub fn min_confidence(cfg: &SpeechConfig) -> f64 {
    cfg.min_confidence()
}

/// Returns the confirmation window in seconds, never negative.
pub fn confirm_seconds(cfg: &SpeechConfig) -> f64 {
    cfg.confirm_seconds()
}

/// Returns the configured language, falling back to `LC_ALL` then `LANG`.
pub fn language(cfg: &SpeechConfig) -> String {
    language_with_locale(
        cfg,
        env_nonempty("LC_ALL").as_deref(),
        env_nonempty("LANG").as_deref(),
    )
}

/// Returns the configured language using an explicit locale instead of the process environment.
///
/// A configured value wins over the locale. Both are reduced to their first
/// component, split on `-` before `_`, exactly as in the Python source.
pub fn language_with_locale(
    cfg: &SpeechConfig,
    lc_all: Option<&str>,
    lang: Option<&str>,
) -> String {
    let configured = py_trim(&cfg.language);
    if !configured.is_empty() {
        let head = configured.split('-').next().unwrap_or("");
        return head.split('_').next().unwrap_or("").to_lowercase();
    }
    let loc = lc_all
        .filter(|value| !value.is_empty())
        .or_else(|| lang.filter(|value| !value.is_empty()))
        .unwrap_or("");
    let loc = loc.split('.').next().unwrap_or("");
    if loc.is_empty() {
        return "en".to_string();
    }
    let head = loc.split('_').next().unwrap_or("");
    if head.is_empty() {
        "en".to_string()
    } else {
        head.to_lowercase()
    }
}

/// Returns the ordered providers for `kind`, honouring mode and disabled entries.
///
/// `env_keys` is the optional credential source used by the callers of the
/// Python module: when it is supplied, providers whose credentials are absent
/// are skipped. Pass `None` for the pure config-driven order from
/// `speech_config.py`. An unknown `kind` yields an empty order.
pub fn provider_order(
    cfg: &SpeechConfig,
    env_keys: Option<&SpeechSecrets>,
    kind: &str,
) -> Vec<String> {
    let (order, cloud, local) = match kind {
        "tts" => (&cfg.tts_order, &CLOUD_TTS[..], &LOCAL_TTS[..]),
        "stt" => (&cfg.stt_order, &CLOUD_STT[..], &LOCAL_STT[..]),
        _ => return Vec::new(),
    };
    order
        .iter()
        .filter(|provider| match cfg.speech_mode {
            SpeechMode::LocalOnly => !cloud.contains(&provider.as_str()),
            SpeechMode::CloudOnly => !local.contains(&provider.as_str()),
            SpeechMode::CloudFirst => true,
        })
        .filter(|provider| !cfg.disabled.iter().any(|name| name == *provider))
        .filter(|provider| match env_keys {
            None => true,
            Some(secrets) => required_credentials(provider.as_str())
                .iter()
                .all(|name| secrets.has(name)),
        })
        .cloned()
        .collect()
}

/// Credentials required before a provider can be attempted.
fn required_credentials(provider: &str) -> &'static [&'static str] {
    match provider {
        "deepgram" => &[KEY_NAMES[0]],
        "groq" => &[KEY_NAMES[1]],
        "cloudflare" => &[KEY_NAMES[2], KEY_NAMES[3]],
        "gemini" => &[KEY_NAMES[4]],
        _ => &[],
    }
}

/// Credentials merged from the process environment and a `KEY=VALUE` file.
///
/// Values are never formatted into `Debug` output; use [`Self::key`] to read one.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpeechSecrets {
    pub deepgram_api_key: String,
    pub groq_api_key: String,
    pub cloudflare_account_id: String,
    pub cloudflare_api_token: String,
    pub gemini_api_key: String,
    extra: BTreeMap<String, String>,
}

impl fmt::Debug for SpeechSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let extras: BTreeMap<&str, &'static str> = self
            .extra
            .keys()
            .map(|key| (key.as_str(), "<redacted>"))
            .collect();
        f.debug_struct("SpeechSecrets")
            .field(KEY_NAMES[0], &redact(&self.deepgram_api_key))
            .field(KEY_NAMES[1], &redact(&self.groq_api_key))
            .field(KEY_NAMES[2], &redact(&self.cloudflare_account_id))
            .field(KEY_NAMES[3], &redact(&self.cloudflare_api_token))
            .field(KEY_NAMES[4], &redact(&self.gemini_api_key))
            .field("extra", &extras)
            .finish()
    }
}

impl SpeechSecrets {
    /// Reads secrets from `path`, with process environment values taking precedence.
    ///
    /// An unreadable file is treated as empty, matching the Python `secrets()`
    /// helper. Extra `KEY=VALUE` pairs are kept and exposed through [`Self::key`].
    pub fn load(path: &Path) -> Self {
        let file_values = read_env_file(path);
        let mut secrets = Self {
            deepgram_api_key: known_secret(KEY_NAMES[0], &file_values),
            groq_api_key: known_secret(KEY_NAMES[1], &file_values),
            cloudflare_account_id: known_secret(KEY_NAMES[2], &file_values),
            cloudflare_api_token: known_secret(KEY_NAMES[3], &file_values),
            gemini_api_key: known_secret(KEY_NAMES[4], &file_values),
            extra: BTreeMap::new(),
        };
        for (key, value) in file_values {
            if KEY_NAMES.contains(&key.as_str()) {
                continue;
            }
            // `setdefault` keeps known keys; `os.environ.get(key, value)` lets an
            // explicitly empty environment variable override the file value.
            let value = std::env::var(&key).unwrap_or(value);
            secrets.extra.insert(key, value);
        }
        secrets
    }

    /// Looks up one credential, returning an empty string when it is unset.
    pub fn key(&self, name: &str) -> String {
        self.key_or(name, "")
    }

    /// Looks up one credential, returning `default` when it is unset.
    pub fn key_or(&self, name: &str, default: &str) -> String {
        if let Some(value) = env_nonempty(name) {
            return value;
        }
        let stored = match name {
            "DEEPGRAM_API_KEY" => Some(&self.deepgram_api_key),
            "GROQ_API_KEY" => Some(&self.groq_api_key),
            "CLOUDFLARE_ACCOUNT_ID" => Some(&self.cloudflare_account_id),
            "CLOUDFLARE_API_TOKEN" => Some(&self.cloudflare_api_token),
            "GEMINI_API_KEY" => Some(&self.gemini_api_key),
            _ => self.extra.get(name),
        };
        match stored.filter(|value| !value.is_empty()) {
            Some(value) => value.clone(),
            None => default.to_string(),
        }
    }

    /// Returns whether a non-empty credential is available for `name`.
    pub fn has(&self, name: &str) -> bool {
        !self.key(name).is_empty()
    }
}

fn known_secret(name: &str, file_values: &BTreeMap<String, String>) -> String {
    env_nonempty(name)
        .or_else(|| {
            file_values
                .get(name)
                .filter(|value| !value.is_empty())
                .cloned()
        })
        .unwrap_or_default()
}

fn parse_mode(text: &str) -> Option<SpeechMode> {
    match text {
        "cloud-first" => Some(SpeechMode::CloudFirst),
        "local-only" => Some(SpeechMode::LocalOnly),
        "cloud-only" => Some(SpeechMode::CloudOnly),
        _ => None,
    }
}

fn default_order(cloud: &[&str], local: &[&str]) -> Vec<String> {
    cloud
        .iter()
        .chain(local.iter())
        .map(|name| (*name).to_string())
        .collect()
}

fn parse_config_json(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ Value::Object(_)) => value,
        _ => Value::Object(Map::new()),
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Python `str.strip()` also treats the four ASCII information separators as space.
fn py_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}'))
}

/// Truthiness of the JSON values a Python config entry can hold.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|x| x != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// Best-effort Python `str()` for config values.
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(flag) => {
            if *flag {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Python `int()` for a config value, falling back on an invalid value.
fn py_int(value: &Value, default: i64) -> i64 {
    match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer
            } else if let Some(unsigned) = number.as_u64() {
                i64::try_from(unsigned).unwrap_or(i64::MAX)
            } else if let Some(float) = number.as_f64() {
                if float.is_finite() {
                    float.trunc() as i64
                } else {
                    default
                }
            } else {
                default
            }
        }
        Value::Bool(flag) => i64::from(*flag),
        Value::String(text) => {
            let text = py_trim(text);
            let (negative, rest) = if let Some(rest) = text.strip_prefix('-') {
                (true, rest)
            } else if let Some(rest) = text.strip_prefix('+') {
                (false, rest)
            } else {
                (false, text)
            };
            let digits: String = rest.chars().filter(|&c| c != '_').collect();
            match digits.parse::<i64>() {
                Ok(integer) if negative => integer.checked_neg().unwrap_or(i64::MIN),
                Ok(integer) => integer,
                Err(_) => default,
            }
        }
        _ => default,
    }
}

/// Python `float()` for a config value, falling back on an invalid value.
fn py_float(value: &Value, default: f64) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(default),
        Value::Bool(flag) => f64::from(*flag),
        Value::String(text) => {
            let digits: String = py_trim(text).chars().filter(|&c| c != '_').collect();
            match digits.to_ascii_lowercase().as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
                "-inf" | "-infinity" => f64::NEG_INFINITY,
                "nan" | "+nan" | "-nan" => f64::NAN,
                _ => digits.parse::<f64>().unwrap_or(default),
            }
        }
        _ => default,
    }
}

/// Python `list(value)` for the provider order and disabled fields.
fn py_string_list(value: &Value) -> Vec<String> {
    if !py_truthy(value) {
        return Vec::new();
    }
    match value {
        Value::Array(items) => items.iter().map(py_str).collect(),
        Value::String(text) => text.chars().map(|ch| ch.to_string()).collect(),
        Value::Object(fields) => fields.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn redact(value: &str) -> &'static str {
    if value.is_empty() { "" } else { "<redacted>" }
}

/// Parses the simple `KEY=VALUE` secrets file understood by the Python module.
fn read_env_file(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut values = BTreeMap::new();
    for line in py_splitlines(&text) {
        let line = py_trim(line);
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = py_trim(key);
        if key.is_empty() {
            continue;
        }
        let value = py_trim(value).trim_matches('"').trim_matches('\'');
        values.insert(key.to_string(), value.to_string());
    }
    values
}

/// Python `str.splitlines()`, including the less common Unicode line boundaries.
fn py_splitlines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let width = match byte {
            b'\n' | b'\r' | 0x0b | 0x0c | 0x1c | 0x1d | 0x1e => 1,
            0xc2 if bytes.get(index + 1) == Some(&0x85) => 2,
            0xe2 if bytes.get(index + 1) == Some(&0x80)
                && matches!(bytes.get(index + 2), Some(&0xa8) | Some(&0xa9)) =>
            {
                3
            }
            _ => {
                index += 1;
                continue;
            }
        };
        lines.push(&text[start..index]);
        index += if byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            2
        } else {
            width
        };
        start = index;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "navi_speech_config_{tag}_{}.tmp",
            std::process::id()
        ))
    }

    fn names(providers: &[String]) -> Vec<&str> {
        providers.iter().map(String::as_str).collect()
    }

    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-12
    }

    #[test]
    fn defaults_match_source_module() {
        let cfg = SpeechConfig::default();
        assert_eq!(cfg.speech_mode, SpeechMode::CloudFirst);
        assert_eq!(
            names(&cfg.tts_order),
            ["edge", "cloudflare", "gemini", "piper"]
        );
        assert_eq!(
            names(&cfg.stt_order),
            ["deepgram", "groq", "cloudflare", "gemini"]
        );
        assert!(cfg.disabled.is_empty());
        assert!(near(cfg.timeout_seconds(), 3.0));
        assert!(near(cfg.breaker_seconds(), 300.0));
        assert_eq!(cfg.breaker_failures(), 2);
        assert_eq!(cfg.retry_attempts(), 1);
        assert_eq!(cfg.tts_voice, "en-GB-RyanNeural");
        assert!(cfg.language.is_empty());
        assert!(near(cfg.min_confidence(), 0.6));
        assert!(near(cfg.confirm_seconds(), 1.5));
        assert_eq!(
            serde_json::to_string(&SpeechMode::LocalOnly).expect("serialize mode"),
            "\"local-only\""
        );
    }

    #[test]
    fn load_merges_file_over_defaults() {
        let path = temp_file("merge");
        std::fs::write(
            &path,
            r#"{
                "speech_mode": "cloud-only",
                "tts_order": ["gemini", "piper", "edge"],
                "stt_order": ["groq"],
                "disabled": ["edge", "groq"],
                "timeout_seconds": "7.5",
                "breaker_seconds": 0,
                "breaker_failures": 0,
                "retry_attempts": -4,
                "tts_voice": "fr-FR-Laure",
                "language": "pt-BR",
                "min_confidence": 0.25,
                "confirm_seconds": -2
            }"#,
        )
        .expect("write config");
        let cfg = load_speech_config(&path).expect("load config");
        assert_eq!(cfg.speech_mode, SpeechMode::CloudOnly);
        assert!(near(cfg.timeout_seconds(), 7.5));
        assert!(near(cfg.breaker_seconds(), 0.0));
        assert_eq!(cfg.breaker_failures(), 1);
        assert_eq!(cfg.retry_attempts(), 0);
        assert!(near(cfg.min_confidence(), 0.25));
        assert!(near(cfg.confirm_seconds(), 0.0));
        assert_eq!(cfg.tts_voice, "fr-FR-Laure");
        assert_eq!(cfg.language(), "pt");
        assert_eq!(names(&cfg.provider_order(None, "tts")), ["gemini"]);
        assert!(cfg.provider_order(None, "stt").is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_values_fall_back_and_accessors_clamp() {
        let cfg = SpeechConfig::from_value(&json!({
            "breaker_failures": "not-a-number",
            "retry_attempts": 12,
            "timeout_seconds": [1, 2],
            "breaker_seconds": "1e3",
            "min_confidence": null,
            "confirm_seconds": "oops"
        }));
        assert_eq!(cfg.breaker_failures(), 2);
        assert_eq!(cfg.retry_attempts(), 12);
        assert!(near(cfg.timeout_seconds(), 3.0));
        assert!(near(cfg.breaker_seconds(), 1_000.0));
        assert!(near(cfg.min_confidence(), 0.6));
        assert!(near(cfg.confirm_seconds(), 1.5));

        let manual = SpeechConfig {
            breaker_failures: -5,
            retry_attempts: -9,
            confirm_seconds: -0.5,
            ..SpeechConfig::default()
        };
        assert_eq!(breaker_failures(&manual), 1);
        assert_eq!(retry_attempts(&manual), 0);
        assert!(near(confirm_seconds(&manual), 0.0));
    }

    #[test]
    fn provider_order_honours_mode_and_disabled() {
        let cfg = SpeechConfig::default();
        assert_eq!(
            names(&provider_order(&cfg, None, "tts")),
            ["edge", "cloudflare", "gemini", "piper"]
        );
        assert_eq!(
            names(&provider_order(&cfg, None, "stt")),
            ["deepgram", "groq", "cloudflare", "gemini"]
        );

        let local = SpeechConfig {
            speech_mode: SpeechMode::LocalOnly,
            ..SpeechConfig::default()
        };
        assert_eq!(names(&local.provider_order(None, "tts")), ["piper"]);
        assert!(local.provider_order(None, "stt").is_empty());

        let cloud = SpeechConfig {
            speech_mode: SpeechMode::CloudOnly,
            ..SpeechConfig::default()
        };
        assert_eq!(
            names(&cloud.provider_order(None, "tts")),
            ["edge", "cloudflare", "gemini"]
        );
        assert_eq!(
            names(&cloud.provider_order(None, "stt")),
            ["deepgram", "groq", "cloudflare", "gemini"]
        );
        assert!(cfg.provider_order(None, "unknown").is_empty());
    }

    #[test]
    fn provider_order_keeps_duplicates_and_credential_filter() {
        let path = temp_file("secrets");
        std::fs::write(
            &path,
            "GEMINI_API_KEY='gem-key'\n# ignored\nNAVI_TEST_SPEECH_TOKEN = \"extra\"\n",
        )
        .expect("write secrets");
        let secrets = SpeechSecrets::load(&path);
        assert!(secrets.has("GEMINI_API_KEY"));
        assert_eq!(secrets.key("NAVI_TEST_SPEECH_TOKEN"), "extra");
        assert_eq!(
            secrets.key_or("NAVI_TEST_SPEECH_MISSING", "fallback"),
            "fallback"
        );

        let cfg = SpeechConfig::default();
        assert_eq!(
            names(&provider_order(&cfg, Some(&secrets), "tts")),
            ["edge", "gemini", "piper"]
        );
        assert_eq!(
            names(&provider_order(&cfg, Some(&secrets), "stt")),
            ["gemini"]
        );

        let custom = SpeechConfig::from_value(&json!({
            "tts_order": ["piper", "edge", "piper", "custom", "edge"],
            "disabled": ["gemini", "piper"]
        }));
        assert_eq!(
            names(&custom.provider_order(None, "tts")),
            ["edge", "custom", "edge"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn language_uses_configured_value_then_locale() {
        let cfg = SpeechConfig::default();
        assert_eq!(language_with_locale(&cfg, None, None), "en");
        assert_eq!(
            language_with_locale(&cfg, Some("de_DE.UTF-8"), Some("fr_FR.UTF-8")),
            "de"
        );
        assert_eq!(language_with_locale(&cfg, None, Some("es_MX.UTF-8")), "es");
        assert_eq!(language_with_locale(&cfg, Some(""), None), "en");

        let configured = SpeechConfig {
            language: " fr-CA ".to_string(),
            ..SpeechConfig::default()
        };
        assert_eq!(language_with_locale(&configured, Some("de_DE"), None), "fr");
        assert_eq!(configured.language(), "fr");
        assert_eq!(
            language(&SpeechConfig {
                language: "pt_BR".to_string(),
                ..SpeechConfig::default()
            }),
            "pt"
        );
    }

    #[test]
    fn missing_or_invalid_file_uses_defaults() {
        let missing = temp_file("missing");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(
            load_speech_config(&missing).expect("missing config"),
            SpeechConfig::default()
        );

        let path = temp_file("invalid");
        std::fs::write(&path, "{not json").expect("write invalid json");
        assert_eq!(
            load_speech_config(&path).expect("invalid json"),
            SpeechConfig::default()
        );
        std::fs::write(&path, "[1, 2, 3]").expect("write non-object json");
        assert_eq!(
            load_speech_config(&path).expect("non-object json"),
            SpeechConfig::default()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn secrets_debug_never_reveals_values() {
        let secrets = SpeechSecrets {
            deepgram_api_key: "dg-secret-token".to_string(),
            gemini_api_key: "gem-secret-token".to_string(),
            ..SpeechSecrets::default()
        };
        let rendered = format!("{secrets:?}");
        assert!(!rendered.contains("dg-secret-token"));
        assert!(!rendered.contains("gem-secret-token"));
        assert!(rendered.contains("DEEPGRAM_API_KEY"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn serde_deserialization_is_tolerant() {
        let cfg: SpeechConfig = serde_json::from_value(json!({
            "speech_mode": "nonsense",
            "tts_order": "ab",
            "breaker_failures": "3",
            "confirm_seconds": "-2"
        }))
        .expect("deserialize config");
        assert_eq!(cfg.speech_mode, SpeechMode::CloudFirst);
        assert_eq!(names(&cfg.tts_order), ["a", "b"]);
        assert_eq!(cfg.breaker_failures(), 3);
        assert!(near(cfg.confirm_seconds(), 0.0));
    }
}
