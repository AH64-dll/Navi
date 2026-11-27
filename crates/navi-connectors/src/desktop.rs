//! Desktop automation commands ported from the desktop worker in
//! `apps/navi/workers/desktop.py`.
//!
//! An action is pure data. Planning turns it into an explicit program and
//! argument vector, so the process that would be started can be inspected
//! before anything runs. Isolated mode rejects every action before planning.

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use navi_types::{NaviError, NaviResult};

/// PipeWire's default sink, as used by the desktop worker's system commands.
const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";

/// A desktop operation requested by a caller or parsed from plain language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopAction {
    /// Open a web address. Only `http` and `https` are accepted.
    OpenUrl(String),
    /// Open a file or directory with the desktop's default handler.
    OpenPath(String),
    /// Change the output volume by a relative percentage.
    VolumeDelta(i32),
    /// Enable or disable muting of the default output sink.
    VolumeMute(bool),
    /// Show a desktop notification with a title and body.
    Notify { title: String, body: String },
    /// Start an application with zero or more arguments.
    LaunchApp { program: String, args: Vec<String> },
}

impl DesktopAction {
    /// Stable name used in execution results.
    fn kind(&self) -> &'static str {
        match self {
            Self::OpenUrl(_) => "open_url",
            Self::OpenPath(_) => "open_path",
            Self::VolumeDelta(_) => "volume_delta",
            Self::VolumeMute(_) => "volume_mute",
            Self::Notify { .. } => "notify",
            Self::LaunchApp { .. } => "launch_app",
        }
    }

    /// Short human-readable outcome for a delivered action.
    fn confirmation(&self) -> &'static str {
        match self {
            Self::OpenUrl(_) => "Opened in the browser.",
            Self::OpenPath(_) => "Opened the path.",
            Self::VolumeDelta(_) => "Volume changed.",
            Self::VolumeMute(true) => "Sound muted.",
            Self::VolumeMute(false) => "Sound on.",
            Self::Notify { .. } => "Notification sent.",
            Self::LaunchApp { .. } => "Application launched.",
        }
    }
}

/// An auditable command: the exact program and arguments to hand to the OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopCommandPlan {
    /// Executable name or path.
    pub program: String,
    /// Arguments in order, without the program itself.
    pub args: Vec<String>,
}

impl DesktopCommandPlan {
    /// Builds a plan from a program and an iterable of string arguments.
    pub fn new<P, A, I>(program: P, args: I) -> Self
    where
        P: Into<String>,
        I: IntoIterator<Item = A>,
        A: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Rejects an empty value with a label naming the offending field.
fn reject_empty(value: &str, label: &str) -> NaviResult<()> {
    if value.is_empty() {
        Err(NaviError::InvalidInput(format!(
            "{label} must not be empty"
        )))
    } else {
        Ok(())
    }
}

/// Rejects empty values and control characters, which cannot be safe arguments.
fn reject_control(value: &str, label: &str) -> NaviResult<()> {
    reject_empty(value, label)?;
    if value.chars().any(char::is_control) {
        Err(NaviError::InvalidInput(format!(
            "{label} contains a control character"
        )))
    } else {
        Ok(())
    }
}

/// Rejects embedded NUL bytes in values that may otherwise contain newlines.
fn reject_nul(value: &str, label: &str) -> NaviResult<()> {
    if value.contains('\0') {
        Err(NaviError::InvalidInput(format!(
            "{label} contains a NUL byte"
        )))
    } else {
        Ok(())
    }
}

/// Confirms an address is a non-empty `http` or `https` URL without whitespace.
fn validate_url(url: &str) -> NaviResult<()> {
    reject_control(url, "URL")?;
    if url.chars().any(char::is_whitespace) {
        return Err(NaviError::InvalidInput(
            "URL must not contain whitespace".into(),
        ));
    }
    let lower = url.to_ascii_lowercase();
    let Some(host) = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    else {
        return Err(NaviError::InvalidInput(
            "only http and https URLs can be opened".into(),
        ));
    };
    let authority = host.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(NaviError::InvalidInput("URL must include a host".into()));
    }
    Ok(())
}

/// Builds the command plan for an action without starting any process.
///
/// Returns [`NaviError::InvalidInput`] for values that cannot be passed safely,
/// in particular for URLs whose scheme is not `http` or `https`.
pub fn plan_desktop_action(action: &DesktopAction) -> NaviResult<DesktopCommandPlan> {
    match action {
        DesktopAction::OpenUrl(url) => {
            validate_url(url)?;
            Ok(DesktopCommandPlan::new("xdg-open", [url.as_str()]))
        }
        DesktopAction::OpenPath(path) => {
            reject_control(path, "path")?;
            Ok(DesktopCommandPlan::new("xdg-open", [path.as_str()]))
        }
        DesktopAction::VolumeDelta(delta) => {
            let sign = if *delta < 0 { "%-" } else { "%+" };
            let step = format!("{}{sign}", delta.unsigned_abs());
            Ok(DesktopCommandPlan::new(
                "wpctl",
                ["set-volume", "-l", "1.0", DEFAULT_SINK, step.as_str()],
            ))
        }
        DesktopAction::VolumeMute(muted) => Ok(DesktopCommandPlan::new(
            "wpctl",
            ["set-mute", DEFAULT_SINK, if *muted { "1" } else { "0" }],
        )),
        DesktopAction::Notify { title, body } => {
            reject_control(title, "notification title")?;
            reject_nul(body, "notification body")?;
            Ok(DesktopCommandPlan::new(
                "notify-send",
                [title.as_str(), body.as_str()],
            ))
        }
        DesktopAction::LaunchApp { program, args } => {
            reject_control(program, "program")?;
            for arg in args {
                reject_nul(arg, "argument")?;
            }
            Ok(DesktopCommandPlan::new(
                program.as_str(),
                args.iter().map(String::as_str),
            ))
        }
    }
}

/// Case-insensitive prefix removal that keeps the original byte offsets valid.
fn strip_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then_some(&text[prefix.len()..])
}

/// Removes a leading command word, requiring a space or colon after it.
fn after_word<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    words.iter().find_map(|word| {
        let rest = strip_ci(text, word)?;
        if rest.starts_with([' ', ':']) {
            Some(rest.trim_start_matches([' ', ':']))
        } else {
            None
        }
    })
}

/// Removes a courtesy lead only when it is a whole word.
fn strip_lead<'a>(text: &'a str, lead: &str) -> Option<&'a str> {
    let rest = strip_ci(text, lead)?;
    if rest.is_empty() || rest.starts_with([' ', ',', ':']) {
        Some(rest.trim_start_matches([' ', ',', ':']))
    } else {
        None
    }
}

/// Strips politeness phrases from a spoken command while keeping inner casing.
fn trim_courtesy(text: &str) -> &str {
    const LEADS: &[&str] = &[
        "hey navi",
        "okay navi",
        "ok navi",
        "navi",
        "i want you to",
        "i need you to",
        "i want to",
        "i need to",
        "i'd like to",
        "i would like to",
        "let's",
        "lets",
        "just",
        "quickly",
        "go ahead and",
        "please",
        "can you",
        "could you",
        "would you",
        "will you",
    ];
    const TAILS: &[&str] = &[" for me", " please", " right now", " now", " navi"];
    let mut current = text.trim().trim_end_matches(['.', '!', '?', ' ']);
    loop {
        let mut stripped = false;
        for lead in LEADS {
            if let Some(rest) = strip_lead(current, lead) {
                current = rest;
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    loop {
        let lower = current.to_ascii_lowercase();
        let mut stripped = false;
        for tail in TAILS {
            if lower.ends_with(tail) {
                current = current[..current.len() - tail.len()].trim_end_matches([' ', ',']);
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    current
}

const VOLUME_UP: &[&str] = &[
    "volume up",
    "turn up the volume",
    "turn the volume up",
    "raise the volume",
    "louder",
    "make it louder",
];
const VOLUME_DOWN: &[&str] = &[
    "volume down",
    "turn down the volume",
    "turn the volume down",
    "lower the volume",
    "quieter",
    "make it quieter",
];
const MUTE_ON: &[&str] = &[
    "mute the sound",
    "mute the volume",
    "mute the audio",
    "mute the speakers",
    "mute my computer",
    "mute the computer",
];
const MUTE_OFF: &[&str] = &[
    "unmute the sound",
    "unmute the volume",
    "unmute the audio",
    "unmute the speakers",
    "unmute my computer",
    "unmute the computer",
];

/// Extensions that name files rather than web hosts, from the desktop worker.
const FILE_TLDS: &[&str] = &[
    "md", "rs", "py", "txt", "pdf", "png", "jpg", "zip", "sh", "pl", "json", "toml", "lock",
];

/// Recognizes an address from phrases such as "open example dot com".
fn parse_url_intent(text: &str) -> Option<String> {
    let rest = after_word(
        text,
        &[
            "open",
            "go to",
            "browse to",
            "navigate to",
            "visit",
            "take me to",
        ],
    )?;
    let candidate = rest.replace(" dot ", ".");
    let candidate = candidate.trim();
    if candidate.contains(' ') || !candidate.contains('.') {
        return None;
    }
    let lower = candidate.to_ascii_lowercase();
    let url = if lower.starts_with("https://") || lower.starts_with("http://") {
        candidate.to_string()
    } else {
        let host = lower.split('/').next().unwrap_or_default();
        let tld = host.rsplit('.').next().unwrap_or_default();
        if tld.len() < 2
            || !tld.chars().all(|c| c.is_ascii_alphabetic())
            || FILE_TLDS.contains(&tld)
        {
            return None;
        }
        format!("https://{candidate}")
    };
    let valid = url
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c));
    if valid { Some(url) } else { None }
}

/// Recognizes absolute, home-relative, and `file://` paths after "open".
fn parse_path_intent(text: &str) -> Option<String> {
    let rest = after_word(text, &["open"])?;
    let candidate = if let Some(path) = rest.strip_prefix("file://") {
        path
    } else {
        let inner = ["file", "folder", "directory"]
            .iter()
            .find_map(|lead| strip_ci(rest, lead))
            .unwrap_or(rest);
        inner.trim_start()
    };
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate.chars().any(char::is_control) {
        return None;
    }
    let path_like = candidate.starts_with('/')
        || candidate.starts_with("~/")
        || candidate.starts_with("./")
        || candidate.starts_with("../")
        || (candidate.contains('/') && !candidate.contains("://"));
    if path_like {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Recognizes "notify …" and "send a notification …" phrasings.
fn parse_notify_intent(text: &str) -> Option<DesktopAction> {
    const LEADS: &[&str] = &[
        "send a notification",
        "show a notification",
        "notify me",
        "notify",
    ];
    let rest = after_word(text, LEADS)?;
    let (title, body) = match rest.split_once(':') {
        Some((title, body)) => {
            let title = title.trim();
            let title = if title.is_empty() { "Navi" } else { title };
            (title.to_string(), body.trim().to_string())
        }
        None => ("Navi".to_string(), rest.trim().to_string()),
    };
    if body.is_empty() {
        return None;
    }
    Some(DesktopAction::Notify { title, body })
}

/// Recognizes explicit application launches, including "open app …".
fn parse_launch_intent(text: &str) -> Option<DesktopAction> {
    const LEADS: &[&str] = &["launch", "start", "run", "open app", "open application"];
    const STOPWORDS: &[&str] = &["the", "a", "an", "my", "your", "this", "that"];
    let rest = after_word(text, LEADS)?;
    let mut parts = rest.split_whitespace();
    let program = parts.next()?.to_string();
    if STOPWORDS.contains(&program.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(DesktopAction::LaunchApp {
        program,
        args: parts.map(str::to_string).collect(),
    })
}

/// Parses a spoken desktop intent, returning `None` when it is not a command.
///
/// Exact volume phrases take precedence over web, path, notification, and
/// application leads, as in the original desktop worker.
pub fn parse_desktop_intent(text: &str) -> Option<DesktopAction> {
    let text = trim_courtesy(text);
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if VOLUME_UP.contains(&lower.as_str()) {
        return Some(DesktopAction::VolumeDelta(10));
    }
    if VOLUME_DOWN.contains(&lower.as_str()) {
        return Some(DesktopAction::VolumeDelta(-10));
    }
    if MUTE_ON.contains(&lower.as_str()) {
        return Some(DesktopAction::VolumeMute(true));
    }
    if MUTE_OFF.contains(&lower.as_str()) {
        return Some(DesktopAction::VolumeMute(false));
    }

    parse_url_intent(text)
        .map(DesktopAction::OpenUrl)
        .or_else(|| parse_path_intent(text).map(DesktopAction::OpenPath))
        .or_else(|| parse_notify_intent(text))
        .or_else(|| parse_launch_intent(text))
}

/// Plans and executes a desktop action, reporting what was run as JSON.
///
/// In isolated mode this fails with [`NaviError::PermissionDenied`] before any
/// command is planned or started. Openers and application launches are
/// detached; system commands must exit successfully.
pub fn execute_desktop_action(action: &DesktopAction, isolated: bool) -> NaviResult<Value> {
    if isolated {
        return Err(NaviError::PermissionDenied(
            "desktop actions are disabled in isolated mode".into(),
        ));
    }
    let plan = plan_desktop_action(action)?;
    let detached = matches!(
        action,
        DesktopAction::OpenUrl(_) | DesktopAction::OpenPath(_) | DesktopAction::LaunchApp { .. }
    );
    let mut command = Command::new(plan.program.as_str());
    command.args(plan.args.iter());
    if detached {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .spawn()
            .map_err(|error| NaviError::Io(format!("cannot start {}: {error}", plan.program)))?;
    } else {
        let output = command
            .output()
            .map_err(|error| NaviError::Io(format!("cannot run {}: {error}", plan.program)))?;
        if !output.status.success() {
            return Err(NaviError::Protocol(format!(
                "{} failed with status {}",
                plan.program, output.status
            )));
        }
    }
    Ok(json!({
        "ok": true,
        "action": action.kind(),
        "program": plan.program,
        "args": plan.args,
        "message": action.confirmation(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn str_args(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    #[test]
    fn test_volume_phrases_map_to_deltas_and_mute() {
        for phrase in [
            "volume up",
            "turn up the volume",
            "turn the volume up",
            "raise the volume",
            "louder",
            "make it louder",
            "please volume up",
            "volume up now",
        ] {
            assert_eq!(
                parse_desktop_intent(phrase),
                Some(DesktopAction::VolumeDelta(10)),
                "{phrase}"
            );
        }
        for phrase in [
            "volume down",
            "turn down the volume",
            "turn the volume down",
            "lower the volume",
            "quieter",
            "make it quieter",
            "ok navi, volume down",
        ] {
            assert_eq!(
                parse_desktop_intent(phrase),
                Some(DesktopAction::VolumeDelta(-10)),
                "{phrase}"
            );
        }
        for phrase in [
            "mute the sound",
            "mute the volume",
            "mute the audio",
            "mute the speakers",
            "mute my computer",
            "mute the computer",
        ] {
            assert_eq!(
                parse_desktop_intent(phrase),
                Some(DesktopAction::VolumeMute(true)),
                "{phrase}"
            );
        }
        for phrase in [
            "unmute the sound",
            "unmute the volume",
            "unmute the audio",
            "unmute the speakers",
            "unmute my computer",
            "unmute the computer",
        ] {
            assert_eq!(
                parse_desktop_intent(phrase),
                Some(DesktopAction::VolumeMute(false)),
                "{phrase}"
            );
        }
        assert_eq!(parse_desktop_intent("mute"), None);
        assert_eq!(parse_desktop_intent("set the volume to 400"), None);
        assert_eq!(parse_desktop_intent(""), None);
    }

    #[test]
    fn test_url_intents_cover_schemes_and_reject_files() {
        let cases = [
            ("open https://example.com/path", "https://example.com/path"),
            ("go to http://example.org", "http://example.org"),
            ("visit example.com", "https://example.com"),
            ("open example dot com", "https://example.com"),
            (
                "take me to news.example.co.uk",
                "https://news.example.co.uk",
            ),
            (
                "browse to example.com/docs?q=1",
                "https://example.com/docs?q=1",
            ),
        ];
        for (said, expected) in cases {
            assert_eq!(
                parse_desktop_intent(said),
                Some(DesktopAction::OpenUrl(expected.to_string())),
                "{said}"
            );
        }
        assert_eq!(parse_desktop_intent("open readme.md"), None);
        assert_eq!(parse_desktop_intent("open notes.txt"), None);
        assert_eq!(parse_desktop_intent("open example.com with spaces"), None);
        assert_eq!(parse_desktop_intent("visit a b.c"), None);
        assert_eq!(
            parse_desktop_intent("open file:///tmp/a.txt"),
            Some(DesktopAction::OpenPath("/tmp/a.txt".into()))
        );
    }

    #[test]
    fn test_path_launch_and_notify_intents_are_distinct() {
        assert_eq!(
            parse_desktop_intent("open /tmp/report.pdf"),
            Some(DesktopAction::OpenPath("/tmp/report.pdf".into()))
        );
        assert_eq!(
            parse_desktop_intent("open ~/Pictures"),
            Some(DesktopAction::OpenPath("~/Pictures".into()))
        );
        assert_eq!(
            parse_desktop_intent("open file:///tmp/report.pdf"),
            Some(DesktopAction::OpenPath("/tmp/report.pdf".into()))
        );
        assert_eq!(
            parse_desktop_intent("open folder /var/log"),
            Some(DesktopAction::OpenPath("/var/log".into()))
        );
        assert_eq!(
            parse_desktop_intent("launch firefox --new-window example.com"),
            Some(DesktopAction::LaunchApp {
                program: "firefox".into(),
                args: vec!["--new-window".into(), "example.com".into()],
            })
        );
        assert_eq!(
            parse_desktop_intent("open app gedit"),
            Some(DesktopAction::LaunchApp {
                program: "gedit".into(),
                args: vec![],
            })
        );
        assert_eq!(parse_desktop_intent("launch the browser"), None);
        assert_eq!(
            parse_desktop_intent("notify Dinner is ready"),
            Some(DesktopAction::Notify {
                title: "Navi".into(),
                body: "Dinner is ready".into(),
            })
        );
        assert_eq!(
            parse_desktop_intent("notify Meeting starts: in five minutes"),
            Some(DesktopAction::Notify {
                title: "Meeting starts".into(),
                body: "in five minutes".into(),
            })
        );
        assert_eq!(
            parse_desktop_intent("notify me: Time is up"),
            Some(DesktopAction::Notify {
                title: "Navi".into(),
                body: "Time is up".into(),
            })
        );
        assert_eq!(
            parse_desktop_intent("send a notification Backup done: all files copied"),
            Some(DesktopAction::Notify {
                title: "Backup done".into(),
                body: "all files copied".into(),
            })
        );
        assert_eq!(parse_desktop_intent("notify:"), None);
    }

    #[test]
    fn test_open_plans_validate_schemes_and_paths() {
        let url = plan_desktop_action(&DesktopAction::OpenUrl("https://example.com/a".into()))
            .expect("valid URL");
        assert_eq!(url.program, "xdg-open");
        assert_eq!(str_args(&url.args), ["https://example.com/a"]);

        let path = plan_desktop_action(&DesktopAction::OpenPath("/tmp/a b.txt".into()))
            .expect("valid path");
        assert_eq!(path.program, "xdg-open");
        assert_eq!(str_args(&path.args), ["/tmp/a b.txt"]);

        for bad in [
            "file:///tmp/x",
            "ftp://example.com",
            "javascript:alert(1)",
            "mailto:someone@example.com",
            "https://",
            "https:///path",
            "http://exa mple.com",
            "example.com",
            "",
        ] {
            assert!(
                plan_desktop_action(&DesktopAction::OpenUrl(bad.into())).is_err(),
                "{bad}"
            );
        }
        assert!(plan_desktop_action(&DesktopAction::OpenPath("".into())).is_err());
        assert!(plan_desktop_action(&DesktopAction::OpenPath("/tmp/x\0y".into())).is_err());
    }

    #[test]
    fn test_volume_plans_cover_signs_and_bounds() {
        let up = plan_desktop_action(&DesktopAction::VolumeDelta(10)).expect("positive delta");
        assert_eq!(up.program, "wpctl");
        assert_eq!(
            str_args(&up.args),
            ["set-volume", "-l", "1.0", DEFAULT_SINK, "10%+"]
        );

        let down = plan_desktop_action(&DesktopAction::VolumeDelta(-5)).expect("negative delta");
        assert_eq!(
            str_args(&down.args),
            ["set-volume", "-l", "1.0", DEFAULT_SINK, "5%-"]
        );

        let zero = plan_desktop_action(&DesktopAction::VolumeDelta(0)).expect("zero delta");
        assert_eq!(str_args(&zero.args).last().copied(), Some("0%+"));
        let min = plan_desktop_action(&DesktopAction::VolumeDelta(i32::MIN)).expect("minimum");
        assert_eq!(str_args(&min.args).last().copied(), Some("2147483648%-"));

        let mute = plan_desktop_action(&DesktopAction::VolumeMute(true)).expect("mute on");
        assert_eq!(mute.program, "wpctl");
        assert_eq!(str_args(&mute.args), ["set-mute", DEFAULT_SINK, "1"]);
        let unmute = plan_desktop_action(&DesktopAction::VolumeMute(false)).expect("mute off");
        assert_eq!(str_args(&unmute.args), ["set-mute", DEFAULT_SINK, "0"]);
    }

    #[test]
    fn test_notify_and_launch_plans_preserve_arguments() {
        let note = plan_desktop_action(&DesktopAction::Notify {
            title: "Hi".into(),
            body: "There".into(),
        })
        .expect("notification");
        assert_eq!(note.program, "notify-send");
        assert_eq!(str_args(&note.args), ["Hi", "There"]);

        let app = plan_desktop_action(&DesktopAction::LaunchApp {
            program: "gtk-launch".into(),
            args: vec!["org.example.App".into(), "--flag".into()],
        })
        .expect("launch");
        assert_eq!(app.program, "gtk-launch");
        assert_eq!(str_args(&app.args), ["org.example.App", "--flag"]);

        assert!(
            plan_desktop_action(&DesktopAction::LaunchApp {
                program: "".into(),
                args: vec![],
            })
            .is_err()
        );
        assert!(
            plan_desktop_action(&DesktopAction::LaunchApp {
                program: "sh".into(),
                args: vec!["\0".into()],
            })
            .is_err()
        );
        assert!(
            plan_desktop_action(&DesktopAction::Notify {
                title: "\0".into(),
                body: "x".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn test_isolated_mode_rejects_every_action() {
        let actions = [
            DesktopAction::OpenUrl("https://example.com".into()),
            DesktopAction::OpenPath("/tmp/x".into()),
            DesktopAction::VolumeDelta(10),
            DesktopAction::VolumeMute(true),
            DesktopAction::Notify {
                title: "t".into(),
                body: "b".into(),
            },
            DesktopAction::LaunchApp {
                program: "navi-no-such-binary-9f2c".into(),
                args: vec![],
            },
        ];
        for action in &actions {
            match execute_desktop_action(action, true) {
                Err(NaviError::PermissionDenied(message)) => {
                    assert!(message.contains("isolated"), "{message}");
                }
                other => panic!("expected permission denied, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_execute_launches_detached_app_and_reports_plan() {
        let action = DesktopAction::LaunchApp {
            program: "/bin/true".into(),
            args: vec![],
        };
        let value = execute_desktop_action(&action, false).expect("detached launch");
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["action"], json!("launch_app"));
        assert_eq!(value["program"], json!("/bin/true"));
        assert_eq!(value["args"], json!([]));
        assert_eq!(value["message"], json!("Application launched."));
    }

    #[test]
    fn test_execute_reports_spawn_failures() {
        let action = DesktopAction::LaunchApp {
            program: "navi-no-such-binary-9f2c".into(),
            args: vec![],
        };
        match execute_desktop_action(&action, false) {
            Err(NaviError::Io(message)) => {
                assert!(message.contains("navi-no-such-binary-9f2c"), "{message}");
            }
            other => panic!("expected io error, got {other:?}"),
        }
    }

    #[test]
    fn test_actions_and_plans_survive_json_roundtrips() {
        let action = DesktopAction::Notify {
            title: "t".into(),
            body: "b".into(),
        };
        let encoded = serde_json::to_string(&action).expect("encode action");
        let decoded: DesktopAction = serde_json::from_str(&encoded).expect("decode action");
        assert_eq!(decoded, action);

        let plan = DesktopCommandPlan::new("p", ["a", "b"]);
        let value = serde_json::to_value(&plan).expect("encode plan");
        assert_eq!(value["program"], json!("p"));
        let decoded: DesktopCommandPlan = serde_json::from_value(value).expect("decode plan");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn test_courtesy_phrases_are_stripped() {
        for said in [
            "Hey Navi, volume up",
            "please volume up",
            "can you volume up",
            "volume up for me",
            "volume up please",
            "I want you to volume up",
        ] {
            assert_eq!(
                parse_desktop_intent(said),
                Some(DesktopAction::VolumeDelta(10)),
                "{said}"
            );
        }
    }

    #[test]
    fn test_plan_new_collects_generic_arguments() {
        let plan = DesktopCommandPlan::new("tool", ["a".to_string(), "b".into()]);
        assert_eq!(plan.program, "tool");
        assert_eq!(plan.args, ["a", "b"]);
    }
}
