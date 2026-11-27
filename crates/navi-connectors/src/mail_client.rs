//! Offline mail worker dispatcher ported from `apps/navi/workers/mail.py`.
//!
//! `navi-connectors` has no socket dependency, so the worker talks to a local
//! JSON Lines mailbox instead of IMAP/SMTP. [`crate::mail`] supplies the
//! RFC 5322 and MIME parsing; this module owns persistence, account settings,
//! and the operation dispatch table. In isolated mode `status` always reports
//! a disconnected account and no operation can reach a real mail server.

#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use navi_types::{NaviError, NaviResult};

use crate::mail::{self, MailSummary};
use crate::persona;

const MAIL_SUBDIR: &str = "mail";
const ACCOUNT_FILE: &str = "account.json";
const MAILBOX_FILE: &str = "mailbox.jsonl";

const DEFAULT_IMAP_PORT: u16 = 993;
const DEFAULT_SMTP_PORT: u16 = 587;

fn default_imap_port() -> u16 {
    DEFAULT_IMAP_PORT
}

fn default_smtp_port() -> u16 {
    DEFAULT_SMTP_PORT
}

fn default_use_tls() -> bool {
    true
}

/// Account settings persisted for the offline worker. Passwords are never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAccountConfig {
    /// Host name used for the account (the IMAP host when derived from an address).
    pub host: String,
    /// Legacy general port, kept in sync with `imap_port`.
    #[serde(default = "default_imap_port")]
    pub port: u16,
    /// User name or full email address used to sign in.
    pub username: String,
    /// IMAP port; defaults to 993.
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    /// SMTP port; defaults to 587.
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Whether implicit TLS is requested for the connection.
    #[serde(default = "default_use_tls")]
    pub use_tls: bool,
}

impl Default for MailAccountConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: DEFAULT_IMAP_PORT,
            username: String::new(),
            imap_port: DEFAULT_IMAP_PORT,
            smtp_port: DEFAULT_SMTP_PORT,
            use_tls: true,
        }
    }
}

/// One record in the JSON Lines mailbox. `raw` is an RFC 5322 message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    /// Stable identifier used by the `read` operation.
    pub id: String,
    /// Complete RFC 5322 message, parsed on demand by [`crate::mail`].
    pub raw: String,
    /// Local read flag; reading a message never marks a real account as read.
    #[serde(default)]
    pub read: bool,
    /// `true` when this record was written by the `draft` operation.
    #[serde(default)]
    pub draft: bool,
}

/// An offline mailbox and its account settings, persisted under `data_dir`.
#[derive(Debug, Clone)]
pub struct MailStore {
    account_path: PathBuf,
    mailbox_path: PathBuf,
    account: Option<MailAccountConfig>,
    messages: Vec<StoredMessage>,
}

impl MailStore {
    /// Loads the account and mailbox from `data_dir`, tolerating absent files
    /// and skipping malformed mailbox lines.
    pub fn open(data_dir: &Path) -> NaviResult<Self> {
        let root = data_dir.join(MAIL_SUBDIR);
        let account_path = root.join(ACCOUNT_FILE);
        let mailbox_path = root.join(MAILBOX_FILE);

        let account = match std::fs::read_to_string(account_path.as_path()) {
            Ok(text) => serde_json::from_str(text.as_str()).ok(),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(NaviError::Io(format!(
                    "cannot read {}: {error}",
                    account_path.display()
                )));
            }
        };

        let mut messages = Vec::new();
        match std::fs::read_to_string(mailbox_path.as_path()) {
            Ok(text) => {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(message) = serde_json::from_str::<StoredMessage>(line) {
                        messages.push(message);
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(NaviError::Io(format!(
                    "cannot read {}: {error}",
                    mailbox_path.display()
                )));
            }
        }

        Ok(Self {
            account_path,
            mailbox_path,
            account,
            messages,
        })
    }

    /// Returns the stored account settings, if any have been configured.
    pub fn config(&self) -> Option<&MailAccountConfig> {
        self.account.as_ref()
    }

    /// Returns every message in the mailbox, in append order.
    pub fn messages(&self) -> &[StoredMessage] {
        self.messages.as_slice()
    }

    /// Finds a message by its identifier.
    pub fn find(&self, id: &str) -> Option<&StoredMessage> {
        self.messages.iter().find(|message| message.id == id)
    }

    /// Returns an unused identifier, one greater than the largest numeric id.
    pub fn next_id(&self) -> String {
        let highest = self
            .messages
            .iter()
            .filter_map(|message| message.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        highest.saturating_add(1).to_string()
    }

    /// Appends a message and rewrites the mailbox file atomically.
    pub fn append(&mut self, message: StoredMessage) -> NaviResult<()> {
        let mut contents = String::new();
        for existing in &self.messages {
            append_line(existing, &mut contents)?;
        }
        append_line(&message, &mut contents)?;
        write_atomic(self.mailbox_path.as_path(), contents.as_str())?;
        self.messages.push(message);
        Ok(())
    }

    /// Persists the account settings, replacing any previous configuration.
    pub fn set_config(&mut self, config: MailAccountConfig) -> NaviResult<()> {
        let text = serde_json::to_string_pretty(&config)
            .map_err(|error| NaviError::Protocol(format!("cannot encode mail config: {error}")))?;
        write_atomic(self.account_path.as_path(), text.as_str())?;
        self.account = Some(config);
        Ok(())
    }
}

/// Appends one JSON line, escaping embedded newlines in the raw message.
fn append_line(message: &StoredMessage, out: &mut String) -> NaviResult<()> {
    let line = serde_json::to_string(message)
        .map_err(|error| NaviError::Protocol(format!("cannot encode mailbox message: {error}")))?;
    out.push_str(line.as_str());
    out.push('\n');
    Ok(())
}

/// Writes through a sibling temporary file so readers never see a partial file.
fn write_atomic(path: &Path, contents: &str) -> NaviResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(temporary.as_path(), contents)?;
    std::fs::rename(temporary.as_path(), path)?;
    Ok(())
}

/// Reads a string parameter, accepting numeric scalars and trimming whitespace.
fn get_string(params: &Value, key: &str) -> Option<String> {
    match params.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// Reads an unsigned 16-bit parameter from a JSON number or decimal string.
fn get_u16(params: &Value, key: &str) -> Option<u16> {
    match params.get(key) {
        Some(Value::Number(number)) => number.as_u64().and_then(|value| u16::try_from(value).ok()),
        Some(Value::String(text)) => text.trim().parse::<u16>().ok(),
        _ => None,
    }
}

/// Reads a boolean parameter, accepting `"true"`, `"yes"`, `1` and their opposites.
fn get_bool(params: &Value, key: &str) -> Option<bool> {
    match params.get(key) {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        Some(Value::Number(number)) => match number.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// Maps a known email domain to its IMAP and SMTP hosts, mirroring `mail.py`.
fn hosts_for(address: &str) -> (String, String) {
    let domain = address
        .rsplit_once('@')
        .map_or(address, |(_, domain)| domain)
        .to_ascii_lowercase();
    let known = match domain.as_str() {
        "gmail.com" | "googlemail.com" => ("imap.gmail.com", "smtp.gmail.com"),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" => {
            ("outlook.office365.com", "smtp-mail.outlook.com")
        }
        "yahoo.com" => ("imap.mail.yahoo.com", "smtp.mail.yahoo.com"),
        "icloud.com" | "me.com" => ("imap.mail.me.com", "smtp.mail.me.com"),
        "aol.com" => ("imap.aol.com", "smtp.aol.com"),
        "fastmail.com" => ("imap.fastmail.com", "smtp.fastmail.com"),
        "zoho.com" => ("imap.zoho.com", "smtp.zoho.com"),
        _ => return (format!("imap.{domain}"), format!("smtp.{domain}")),
    };
    (known.0.to_string(), known.1.to_string())
}

/// Builds account settings from request parameters, keeping existing values
/// for fields the caller omitted.
fn config_from_params(
    params: &Value,
    existing: Option<&MailAccountConfig>,
) -> NaviResult<MailAccountConfig> {
    let base = existing.cloned().unwrap_or_default();
    let address = get_string(params, "address");
    let username = get_string(params, "username")
        .or_else(|| address.clone())
        .unwrap_or(base.username);
    let host = get_string(params, "host")
        .or_else(|| get_string(params, "imap_host"))
        .or_else(|| address.as_deref().map(|value| hosts_for(value).0))
        .unwrap_or(base.host);
    if host.trim().is_empty() {
        return Err(NaviError::InvalidInput(
            "a mail host or email address is required".into(),
        ));
    }
    if username.trim().is_empty() {
        return Err(NaviError::InvalidInput(
            "a mail username is required".into(),
        ));
    }
    let port = get_u16(params, "port").unwrap_or(base.port);
    let imap_port = get_u16(params, "imap_port").unwrap_or(port);
    let smtp_port = get_u16(params, "smtp_port").unwrap_or(base.smtp_port);
    let use_tls = get_bool(params, "use_tls").unwrap_or(base.use_tls);
    Ok(MailAccountConfig {
        host,
        port,
        username,
        imap_port,
        smtp_port,
        use_tls,
    })
}

/// Renders account settings as the payload of `status` and `configure`.
fn config_json(config: &MailAccountConfig, connected: bool) -> Value {
    json!({
        "connected": connected,
        "host": config.host,
        "port": config.port,
        "username": config.username,
        "imap_port": config.imap_port,
        "smtp_port": config.smtp_port,
        "use_tls": config.use_tls,
    })
}

/// Collapses whitespace so a header value cannot inject new headers.
fn header_value(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Builds a plain-text RFC 5322 message for a locally saved draft.
fn build_draft_raw(from: &str, to: &str, subject: &str, body: &str) -> String {
    let normalised_body = body.replace("\r\n", "\n").replace('\r', "\n");
    format!(
        "From: {}\r\nTo: {}\r\nSubject: {}\r\nContent-Type: text/plain; charset=utf-8\r\nX-Navi-Draft: 1\r\n\r\n{}\r\n",
        header_value(from),
        header_value(to),
        header_value(subject),
        normalised_body
    )
}

/// Attaches local flags to the summary produced by the mail parser.
fn summary_json(summary: &MailSummary, message: &StoredMessage) -> Value {
    let mut value = serde_json::to_value(summary).unwrap_or_default();
    if let Value::Object(map) = &mut value {
        map.insert("read".to_string(), Value::Bool(message.read));
        map.insert("draft".to_string(), Value::Bool(message.draft));
    }
    value
}

/// Parses one stored message into a JSON summary.
fn message_json(message: &StoredMessage) -> Value {
    summary_json(
        &mail::parse_raw_email(message.id.as_str(), message.raw.as_str()),
        message,
    )
}

fn op_status(data_dir: &Path, isolated: bool) -> NaviResult<Value> {
    if isolated {
        return Ok(json!({"connected": false}));
    }
    let store = MailStore::open(data_dir)?;
    let Some(config) = store.config() else {
        return Ok(json!({"connected": false}));
    };
    Ok(config_json(config, true))
}

fn op_configure(params: &Value, data_dir: &Path) -> NaviResult<Value> {
    let mut store = MailStore::open(data_dir)?;
    let config = config_from_params(params, store.config())?;
    store.set_config(config.clone())?;
    Ok(config_json(&config, true))
}

fn op_list(data_dir: &Path) -> NaviResult<Value> {
    let store = MailStore::open(data_dir)?;
    let messages: Vec<Value> = store.messages().iter().map(message_json).collect();
    Ok(json!({"messages": messages, "count": messages.len()}))
}

fn op_read(params: &Value, data_dir: &Path) -> NaviResult<Value> {
    let store = MailStore::open(data_dir)?;
    let requested = get_string(params, "id")
        .or_else(|| get_string(params, "uid"))
        .or_else(|| get_string(params, "message_id"));
    let message = match requested {
        Some(id) => store.find(id.as_str()),
        None => store.messages().last(),
    };
    let Some(message) = message else {
        return Ok(json!({
            "text": "Your inbox is empty.",
            "speech": "Your inbox is empty.",
        }));
    };
    let summary = mail::parse_raw_email(message.id.as_str(), message.raw.as_str());
    let body = mail::body_text(message.raw.as_str());
    let spoken = persona::first_sentences(body.as_str(), 4, 500);
    let said = if spoken.is_empty() {
        "It has no text.".to_string()
    } else {
        spoken
    };
    Ok(json!({
        "id": summary.uid,
        "from": summary.from.display_name(),
        "subject": summary.subject,
        "text": mail::format_read_text(&summary),
        "speech": mail::format_read_speech(&summary, said.as_str()),
    }))
}

fn op_search(params: &Value, data_dir: &Path) -> NaviResult<Value> {
    let store = MailStore::open(data_dir)?;
    let query = get_string(params, "query")
        .or_else(|| get_string(params, "q"))
        .or_else(|| get_string(params, "text"))
        .unwrap_or_default()
        .to_lowercase();
    let mut messages: Vec<Value> = Vec::new();
    for message in store.messages() {
        let summary = mail::parse_raw_email(message.id.as_str(), message.raw.as_str());
        let haystack = format!(
            "{} {} {} {} {} {}",
            summary.from.name,
            summary.from.address,
            summary.to.name,
            summary.to.address,
            summary.subject,
            summary.snippet
        )
        .to_lowercase();
        if query.is_empty() || haystack.contains(query.as_str()) {
            messages.push(summary_json(&summary, message));
        }
    }
    Ok(json!({"messages": messages, "count": messages.len()}))
}

fn op_draft(params: &Value, data_dir: &Path) -> NaviResult<Value> {
    let mut store = MailStore::open(data_dir)?;
    let to = get_string(params, "to").unwrap_or_default();
    let subject = get_string(params, "subject").unwrap_or_default();
    let body = match params.get("body") {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    };
    let from = get_string(params, "from").unwrap_or_else(|| {
        store
            .config()
            .map_or_else(String::new, |config| config.username.clone())
    });
    let id = store.next_id();
    let raw = build_draft_raw(from.as_str(), to.as_str(), subject.as_str(), body.as_str());
    store.append(StoredMessage {
        id: id.clone(),
        raw,
        read: true,
        draft: true,
    })?;
    Ok(json!({"draft": true, "id": id, "to": to, "subject": subject}))
}

/// Dispatches one mail operation against the offline store in `data_dir`.
///
/// `status` is the only operation that reads the isolated flag: in isolated
/// mode it reports `connected: false` without inspecting any account. Every
/// other operation works solely on local files, so no call reaches a network.
pub fn run_mail_op(op: &str, params: &Value, data_dir: &Path, isolated: bool) -> NaviResult<Value> {
    match op {
        "status" => op_status(data_dir, isolated),
        "configure" => op_configure(params, data_dir),
        "list" => op_list(data_dir),
        "read" => op_read(params, data_dir),
        "search" => op_search(params, data_dir),
        "draft" => op_draft(params, data_dir),
        other => Err(NaviError::InvalidInput(format!("Unknown mail op {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unique data directory removed on drop, so tests never share state.
    struct TestDataDir(PathBuf);

    impl TestDataDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "navi-mail-client-{label}-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(path.as_path()).expect("create test data dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            self.0.as_path()
        }
    }

    impl Drop for TestDataDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn draft(dir: &Path, subject: &str, body: &str) -> Value {
        run_mail_op(
            "draft",
            &json!({
                "to": "bob@example.com",
                "subject": subject,
                "body": body,
            }),
            dir,
            false,
        )
        .expect("draft succeeds")
    }

    #[test]
    fn configure_then_status_reports_the_saved_account() {
        let dir = TestDataDir::new("configure");
        let configured = run_mail_op(
            "configure",
            &json!({
                "host": "imap.example.com",
                "port": 993,
                "username": "alice@example.com",
                "smtp_port": 465,
                "use_tls": false,
            }),
            dir.path(),
            false,
        )
        .expect("configure succeeds");
        assert_eq!(configured["connected"], json!(true));
        assert_eq!(configured["host"], json!("imap.example.com"));
        assert_eq!(configured["smtp_port"], json!(465));
        assert_eq!(configured["use_tls"], json!(false));

        let status = run_mail_op("status", &json!({}), dir.path(), false).expect("status succeeds");
        assert_eq!(status, configured);
        assert_eq!(status["username"], json!("alice@example.com"));
        assert!(status.get("password").is_none());

        let store = MailStore::open(dir.path()).expect("store reloads");
        let config = store.config().expect("config persisted");
        assert_eq!(config.imap_port, 993);
        assert_eq!(config.smtp_port, 465);
    }

    #[test]
    fn isolated_status_never_reports_a_connection() {
        let dir = TestDataDir::new("isolated");
        run_mail_op(
            "configure",
            &json!({"address": "alice@gmail.com"}),
            dir.path(),
            false,
        )
        .expect("configure succeeds");
        let status =
            run_mail_op("status", &json!({}), dir.path(), true).expect("isolated status succeeds");
        assert_eq!(status, json!({"connected": false}));
        assert!(status.get("host").is_none());
        assert!(status.get("username").is_none());
    }

    #[test]
    fn status_without_a_configured_account_reports_disconnected() {
        let dir = TestDataDir::new("unconfigured");
        let status = run_mail_op("status", &json!({}), dir.path(), false).expect("status succeeds");
        assert_eq!(status, json!({"connected": false}));
        assert!(status.get("host").is_none());
        assert!(status.get("username").is_none());
    }

    #[test]
    fn configure_merges_partial_updates_over_the_saved_account() {
        let dir = TestDataDir::new("merge");
        run_mail_op(
            "configure",
            &json!({
                "host": "imap.example.com",
                "username": "alice@example.com",
                "smtp_port": 465,
            }),
            dir.path(),
            false,
        )
        .expect("first configure succeeds");

        let updated = run_mail_op(
            "configure",
            &json!({"host": "imap.new.example"}),
            dir.path(),
            false,
        )
        .expect("second configure succeeds");
        assert_eq!(updated["host"], json!("imap.new.example"));
        assert_eq!(updated["username"], json!("alice@example.com"));
        assert_eq!(updated["smtp_port"], json!(465));
        assert_eq!(updated["imap_port"], json!(993));
        assert_eq!(updated["use_tls"], json!(true));
    }

    #[test]
    fn configure_derives_hosts_and_default_ports_from_address() {
        let dir = TestDataDir::new("derive");
        let configured = run_mail_op(
            "configure",
            &json!({"address": "alice@outlook.com"}),
            dir.path(),
            false,
        )
        .expect("configure succeeds");
        assert_eq!(configured["host"], json!("outlook.office365.com"));
        assert_eq!(configured["username"], json!("alice@outlook.com"));
        assert_eq!(configured["port"], json!(993));
        assert_eq!(configured["imap_port"], json!(993));
        assert_eq!(configured["smtp_port"], json!(587));
        assert_eq!(configured["use_tls"], json!(true));

        let unknown = run_mail_op(
            "configure",
            &json!({"address": "alice@example.org"}),
            dir.path(),
            false,
        )
        .expect("configure succeeds");
        assert_eq!(unknown["host"], json!("imap.example.org"));
    }

    #[test]
    fn draft_is_appended_and_list_reports_it() {
        let dir = TestDataDir::new("draft");
        let created = draft(dir.path(), "Hello there", "First line of the body.");
        assert_eq!(created["draft"], json!(true));
        assert_eq!(created["id"], json!("1"));

        let listed = run_mail_op("list", &json!({}), dir.path(), false).expect("list succeeds");
        assert_eq!(listed["count"], json!(1));
        let messages = listed["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["subject"], json!("Hello there"));
        assert_eq!(messages[0]["draft"], json!(true));
        assert_eq!(messages[0]["read"], json!(true));
        assert_eq!(messages[0]["snippet"], json!("First line of the body."));

        draft(dir.path(), "Second note", "Another body.");
        let listed = run_mail_op("list", &Value::Null, dir.path(), false).expect("list succeeds");
        assert_eq!(listed["count"], json!(2));
    }

    #[test]
    fn read_resolves_ids_and_falls_back_to_the_latest_message() {
        let dir = TestDataDir::new("read");
        run_mail_op(
            "configure",
            &json!({"host": "imap.example.com", "username": "alice"}),
            dir.path(),
            false,
        )
        .expect("configure succeeds");
        draft(dir.path(), "First note", "First body text.");
        draft(dir.path(), "Second note", "Second body text.");

        let by_id = run_mail_op("read", &json!({"id": "1"}), dir.path(), false)
            .expect("read by id succeeds");
        assert_eq!(by_id["id"], json!("1"));
        assert_eq!(by_id["subject"], json!("First note"));
        assert!(
            by_id["text"]
                .as_str()
                .is_some_and(|text| text.contains("First body text."))
        );
        assert!(
            by_id["speech"]
                .as_str()
                .is_some_and(|text| text.contains("First note"))
        );

        let latest =
            run_mail_op("read", &json!({}), dir.path(), false).expect("read latest succeeds");
        assert_eq!(latest["id"], json!("2"));
        assert_eq!(latest["subject"], json!("Second note"));
    }

    #[test]
    fn read_of_an_empty_mailbox_is_not_an_error() {
        let dir = TestDataDir::new("empty");
        let result = run_mail_op("read", &json!({}), dir.path(), false).expect("read succeeds");
        assert_eq!(result["text"], json!("Your inbox is empty."));
        let listed = run_mail_op("list", &json!({}), dir.path(), false).expect("list succeeds");
        assert_eq!(listed["count"], json!(0));
    }

    #[test]
    fn search_matches_subject_sender_and_body() {
        let dir = TestDataDir::new("search");
        draft(dir.path(), "Budget report", "Quarterly numbers are ready.");
        draft(dir.path(), "Dinner plans", "Meet Bob at seven.");

        let by_subject = run_mail_op("search", &json!({"query": "budget"}), dir.path(), false)
            .expect("search succeeds");
        assert_eq!(by_subject["count"], json!(1));
        assert_eq!(by_subject["messages"][0]["subject"], json!("Budget report"));

        let by_recipient = run_mail_op(
            "search",
            &json!({"q": "bob@example.com"}),
            dir.path(),
            false,
        )
        .expect("search succeeds");
        assert_eq!(by_recipient["count"], json!(2));

        let by_body = run_mail_op("search", &json!({"query": "QUARTERLY"}), dir.path(), false)
            .expect("search succeeds");
        assert_eq!(by_body["count"], json!(1));

        let missing = run_mail_op(
            "search",
            &json!({"query": "nothing matches"}),
            dir.path(),
            false,
        )
        .expect("search succeeds");
        assert_eq!(missing["count"], json!(0));

        let all = run_mail_op("search", &json!({}), dir.path(), false).expect("search succeeds");
        assert_eq!(all["count"], json!(2));
    }

    #[test]
    fn unknown_op_and_bad_config_are_reported_as_errors() {
        let dir = TestDataDir::new("errors");
        let unknown = run_mail_op("send", &json!({}), dir.path(), false).expect_err("unknown op");
        assert!(matches!(unknown, NaviError::InvalidInput(_)));
        assert!(unknown.to_string().contains("Unknown mail op send"));

        let missing_host =
            run_mail_op("configure", &json!({}), dir.path(), false).expect_err("missing host");
        assert!(matches!(missing_host, NaviError::InvalidInput(_)));
    }

    #[test]
    fn store_skips_malformed_jsonl_lines_and_generates_ids() {
        let dir = TestDataDir::new("corrupt");
        let mailbox = dir.path().join(MAIL_SUBDIR).join(MAILBOX_FILE);
        std::fs::create_dir_all(mailbox.parent().expect("parent")).expect("create mail dir");
        let good = StoredMessage {
            id: "41".to_string(),
            raw: "Subject: kept\n\nBody.\n".to_string(),
            read: false,
            draft: false,
        };
        let contents = format!(
            "{{not json}}\n{}\n\n{{\"id\":42,\"raw\":42}}\n",
            serde_json::to_string(&good).expect("encode message")
        );
        std::fs::write(mailbox.as_path(), contents).expect("write mailbox");

        let store = MailStore::open(dir.path()).expect("open tolerates bad lines");
        assert_eq!(store.messages().len(), 1);
        assert_eq!(store.messages()[0].id, "41");
        assert_eq!(store.next_id(), "42");
        assert!(store.find("41").is_some());
        assert!(store.find("missing").is_none());
    }

    #[test]
    fn header_values_cannot_inject_extra_headers() {
        assert_eq!(
            header_value("Hi\r\nBcc: victim@example.com"),
            "Hi Bcc: victim@example.com"
        );
        assert_eq!(header_value("  a\t b\n c  "), "a b c");
        let raw = build_draft_raw(
            "alice@example.com",
            "bob@example.com",
            "Re: hi\nBcc: x@y.z",
            "a\nb",
        );
        let header_block = raw.split("\r\n\r\n").next().expect("headers");
        assert_eq!(
            header_block
                .lines()
                .filter(|line| line.starts_with("Bcc:"))
                .count(),
            0
        );
        assert!(header_block.contains("Re: hi Bcc: x@y.z"));
        assert!(raw.contains("a\nb"));
    }
}
