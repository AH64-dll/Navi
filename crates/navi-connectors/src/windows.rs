//! Window enumeration and focus actions for X11 (`wmctrl`) and Hyprland
//! (`hyprctl`), ported from `apps/navi/workers/windows.py`.
//!
//! The Python worker filtered special windows, published at most 100 records
//! and clipped every caption to 200 characters. This module keeps those limits
//! and exposes the same six-field record for both window managers. Parsing is
//! pure and testable without a compositor; process spawning is confined to the
//! two `*_live` functions at the bottom of the file.

#![forbid(unsafe_code)]

use std::process::Command;

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Character limit applied to window captions by the KWin adapter.
const MAX_TITLE_CHARS: usize = 200;
/// Upper bound on the number of windows the Python worker published.
const MAX_WINDOWS: usize = 100;

/// One observable window, independent of the window manager that reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Window id: a `0x` address from `wmctrl` or `hyprctl`.
    pub id: String,
    /// Desktop or workspace index; `-1` means sticky in `wmctrl`.
    pub desktop: i32,
    /// Owner process id, or `0` when the window manager does not report one.
    pub pid: i32,
    /// Host that owns the window; empty for Hyprland clients.
    pub host: String,
    /// Window caption, clipped to [`MAX_TITLE_CHARS`] characters.
    pub title: String,
    /// Application class, for example `firefox.Firefox` or `firefox`.
    pub app_class: String,
}

/// Parses the text output of `wmctrl -l`, `-lp`, `-lx` or `-lpx`.
///
/// Malformed lines are skipped. A numeric token after the desktop is taken as
/// the pid; a hyphenated non-integer token in that slot is a malformed numeric
/// field and discards the record. A dotted token before the host is taken as
/// the application class. The remaining tokens form the title, which may
/// contain spaces.
pub fn parse_wmctrl_output(raw: &str) -> Vec<WindowInfo> {
    let mut windows = Vec::new();
    for line in raw.lines() {
        if windows.len() >= MAX_WINDOWS {
            break;
        }
        if let Some(window) = parse_wmctrl_line(line) {
            windows.push(window);
        }
    }
    windows
}

fn parse_wmctrl_line(line: &str) -> Option<WindowInfo> {
    let mut fields = line.split_whitespace();
    let id = fields.next()?;
    let desktop = fields.next()?.parse::<i32>().ok()?;
    let rest: Vec<&str> = fields.collect();
    if rest.is_empty() {
        return None;
    }

    let mut index = 0;
    let mut pid = 0;
    let first = rest[index];
    if is_integer_token(first) {
        pid = first.parse::<i32>().unwrap_or(0);
        index += 1;
    } else if !first.contains('.') && first.contains('-') {
        // A hyphenated token that is neither a pid nor a dotted class is a
        // malformed numeric field; treating it as a host would silently
        // shift the remaining fields into the title.
        return None;
    }
    if index >= rest.len() {
        return None;
    }

    let mut app_class = String::new();
    if rest.len() - index >= 3 && rest[index].contains('.') {
        app_class = rest[index].to_owned();
        index += 1;
    }
    if index >= rest.len() {
        return None;
    }

    let host = rest[index].to_owned();
    index += 1;
    let title = clip_title(&rest[index..].join(" "));
    Some(WindowInfo {
        id: id.to_owned(),
        desktop,
        pid,
        host,
        title,
        app_class,
    })
}

fn is_integer_token(token: &str) -> bool {
    let digits = token.strip_prefix('-').unwrap_or(token);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn clip_title(title: &str) -> String {
    title.chars().take(MAX_TITLE_CHARS).collect()
}

/// Parses the JSON array printed by `hyprctl -j clients`.
///
/// An object with a `clients` array is accepted as well, so the output of
/// `hyprctl clients` wrappers can be fed in unchanged. Missing fields fall back
/// to empty strings and zeroes instead of failing the whole batch.
pub fn parse_hyprctl_clients(raw: &str) -> NaviResult<Vec<WindowInfo>> {
    let value: Value = serde_json::from_str(raw)?;
    let clients = match &value {
        Value::Array(items) => items.as_slice(),
        Value::Object(object) => match object.get("clients") {
            Some(Value::Array(items)) => items.as_slice(),
            _ => {
                return Err(NaviError::Protocol(
                    "hyprctl clients: expected a JSON array".into(),
                ));
            }
        },
        _ => {
            return Err(NaviError::Protocol(
                "hyprctl clients: expected a JSON array".into(),
            ));
        }
    };

    let mut windows = Vec::new();
    for client in clients {
        if windows.len() >= MAX_WINDOWS {
            break;
        }
        if let Some(object) = client.as_object() {
            windows.push(hyprctl_window(object));
        }
    }
    Ok(windows)
}

fn hyprctl_window(object: &Map<String, Value>) -> WindowInfo {
    let id = string_field(object, "address")
        .or_else(|| string_field(object, "id"))
        .unwrap_or_default();
    let host = string_field(object, "host")
        .or_else(|| string_field(object, "hostname"))
        .unwrap_or_default();
    let title = string_field(object, "title")
        .map(|title| clip_title(&title))
        .unwrap_or_default();
    WindowInfo {
        id,
        desktop: hyprctl_desktop(object),
        pid: number_field(object.get("pid")).unwrap_or(0),
        host,
        title,
        app_class: string_field(object, "class").unwrap_or_default(),
    }
}

fn hyprctl_desktop(object: &Map<String, Value>) -> i32 {
    if let Some(desktop) = number_field(object.get("workspaceId")) {
        return desktop;
    }
    match object.get("workspace") {
        Some(Value::Object(workspace)) => {
            if let Some(desktop) = number_field(workspace.get("id")) {
                return desktop;
            }
            match string_field(workspace, "name") {
                Some(name) => name.trim().parse::<i32>().unwrap_or(0),
                None => 0,
            }
        }
        Some(value) => number_field(Some(value)).unwrap_or(0),
        None => 0,
    }
}

fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    match object.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn number_field(value: Option<&Value>) -> Option<i32> {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .or_else(|| number.as_u64().and_then(|value| i32::try_from(value).ok())),
        Some(Value::String(text)) => text.trim().parse::<i32>().ok(),
        _ => None,
    }
}

/// Finds the best window for a query using id, title and class matching.
///
/// Exact matches outrank substring matches, which outrank ordered-subsequence
/// (fuzzy) matches. Comparisons are case-insensitive and ties keep the first
/// window in the slice. An empty or whitespace-only query matches nothing.
pub fn find_matching_window<'a>(windows: &'a [WindowInfo], query: &str) -> Option<&'a WindowInfo> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }

    let mut best: Option<(&'a WindowInfo, u8)> = None;
    for window in windows {
        let score = match_score(window, &needle);
        if score == 0 {
            continue;
        }
        let replace = match best {
            None => true,
            Some((_, best_score)) => score > best_score,
        };
        if replace {
            best = Some((window, score));
        }
    }
    best.map(|(window, _)| window)
}

fn match_score(window: &WindowInfo, needle: &str) -> u8 {
    let id = window.id.to_lowercase();
    let title = window.title.to_lowercase();
    let class = window.app_class.to_lowercase();

    if id == needle {
        return 100;
    }
    if title == needle {
        return 90;
    }
    if class == needle {
        return 80;
    }
    if title.contains(needle) {
        return 70;
    }
    if class.contains(needle) {
        return 60;
    }
    if id.contains(needle) {
        return 50;
    }
    if is_subsequence(needle, &title) {
        return 40;
    }
    if is_subsequence(needle, &class) {
        return 30;
    }
    if is_subsequence(needle, &id) {
        return 20;
    }
    0
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut remaining = haystack.chars();
    for expected in needle.chars() {
        if remaining.find(|found| *found == expected).is_none() {
            return false;
        }
    }
    true
}

/// Lists windows by querying `hyprctl` when available and falling back to
/// `wmctrl`. Returns a transport error when neither tool can be started.
pub fn list_windows_live() -> NaviResult<Vec<WindowInfo>> {
    if let Ok(windows) = hyprctl_clients_live() {
        return Ok(windows);
    }
    wmctrl_windows_live()
}

fn hyprctl_clients_live() -> NaviResult<Vec<WindowInfo>> {
    let output = Command::new("hyprctl")
        .args(["-j", "clients"])
        .output()
        .map_err(|error| NaviError::Transport(format!("cannot run hyprctl: {error}")))?;
    if !output.status.success() {
        return Err(NaviError::Transport(format!(
            "hyprctl exited with {}",
            output.status
        )));
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|error| NaviError::Protocol(format!("hyprctl output is not UTF-8: {error}")))?;
    parse_hyprctl_clients(&raw)
}

fn wmctrl_windows_live() -> NaviResult<Vec<WindowInfo>> {
    let output = Command::new("wmctrl")
        .args(["-lpx"])
        .output()
        .map_err(|error| NaviError::Transport(format!("cannot run wmctrl: {error}")))?;
    if !output.status.success() {
        return Err(NaviError::Transport(format!(
            "wmctrl exited with {}",
            output.status
        )));
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|error| NaviError::Protocol(format!("wmctrl output is not UTF-8: {error}")))?;
    Ok(parse_wmctrl_output(&raw))
}

/// Focuses a window through `wmctrl` or Hyprland's `focuswindow` dispatch.
///
/// The handle is validated before any process is spawned, so shell metacharacters
/// and stray text can never reach a command line.
pub fn focus_window_live(window_id: &str) -> NaviResult<()> {
    let id = normalize_window_id(window_id)?;

    if let Ok(output) = Command::new("wmctrl").args(["-i", "-a", &id]).output()
        && output.status.success()
    {
        return Ok(());
    }

    let address = format!("address:{id}");
    let output = Command::new("hyprctl")
        .args(["dispatch", "focuswindow", &address])
        .output()
        .map_err(|error| NaviError::Transport(format!("cannot focus window: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(NaviError::Transport(format!(
            "cannot focus window {window_id}: hyprctl exited with {}",
            output.status
        )))
    }
}

fn normalize_window_id(window_id: &str) -> NaviResult<String> {
    let trimmed = window_id.trim();
    let unbraced = trimmed
        .strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
        .unwrap_or(trimmed);
    let core = unbraced
        .strip_prefix("0x")
        .or_else(|| unbraced.strip_prefix("0X"))
        .unwrap_or(unbraced);
    let valid = !core.is_empty()
        && core.len() <= 64
        && core.chars().any(|character| character.is_ascii_hexdigit())
        && core
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-');
    if !valid {
        return Err(NaviError::InvalidInput(format!(
            "invalid native window handle: {window_id}"
        )));
    }
    Ok(core.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_window(id: &str, title: &str, class: &str, desktop: i32) -> WindowInfo {
        WindowInfo {
            id: id.into(),
            desktop,
            pid: 100 + desktop,
            host: "myhost".into(),
            title: title.into(),
            app_class: class.into(),
        }
    }

    #[test]
    fn parse_wmctrl_output_handles_plain_listing() {
        let raw = concat!(
            "0x00000001  0 myhost Terminal\n",
            "\n",
            "  0x00000002  1 myhost Firefox Browser  \r\n",
            "bad line\n",
        );
        let windows = parse_wmctrl_output(raw);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "0x00000001");
        assert_eq!(windows[0].desktop, 0);
        assert_eq!(windows[0].pid, 0);
        assert_eq!(windows[0].host, "myhost");
        assert_eq!(windows[0].title, "Terminal");
        assert!(windows[0].app_class.is_empty());
        assert_eq!(windows[1].id, "0x00000002");
        assert_eq!(windows[1].desktop, 1);
        assert_eq!(windows[1].title, "Firefox Browser");
    }

    #[test]
    fn parse_wmctrl_output_handles_pid_and_class() {
        let raw = "0x12345678  2  4242  firefox.Firefox  myhost  Mozilla Firefox\n";
        let windows = parse_wmctrl_output(raw);
        assert_eq!(windows.len(), 1);
        let window = &windows[0];
        assert_eq!(window.id, "0x12345678");
        assert_eq!(window.desktop, 2);
        assert_eq!(window.pid, 4242);
        assert_eq!(window.app_class, "firefox.Firefox");
        assert_eq!(window.host, "myhost");
        assert_eq!(window.title, "Mozilla Firefox");
    }

    #[test]
    fn parse_wmctrl_output_handles_class_without_pid_and_empty_title() {
        let raw = concat!(
            "0xaa  0 code.Code  workstation  Visual Studio Code\n",
            "0xbb -1 host\n",
            "0xcc 0 not-a-number host Broken\n",
            "0xdd\n",
        );
        let windows = parse_wmctrl_output(raw);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].app_class, "code.Code");
        assert_eq!(windows[0].pid, 0);
        assert_eq!(windows[0].title, "Visual Studio Code");
        assert_eq!(windows[1].desktop, -1);
        assert_eq!(windows[1].host, "host");
        assert!(windows[1].title.is_empty());
    }

    #[test]
    fn parse_hyprctl_clients_parses_typical_json() {
        let raw = r#"[
            {"address":"0xabc123","mapped":true,"workspace":{"id":3,"name":"3"},"pid":321,"class":"firefox","title":"Mozilla Firefox"},
            {"address":"0xdef456","workspace":{"id":1,"name":"1"},"pid":7,"class":"Alacritty","title":"Terminal"}
        ]"#;
        let windows = parse_hyprctl_clients(raw).expect("valid client JSON");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "0xabc123");
        assert_eq!(windows[0].desktop, 3);
        assert_eq!(windows[0].pid, 321);
        assert!(windows[0].host.is_empty());
        assert_eq!(windows[0].title, "Mozilla Firefox");
        assert_eq!(windows[0].app_class, "firefox");
        assert_eq!(windows[1].desktop, 1);
        assert_eq!(windows[1].app_class, "Alacritty");
    }

    #[test]
    fn parse_hyprctl_clients_rejects_bad_payloads_and_ignores_non_objects() {
        assert!(parse_hyprctl_clients("not json").is_err());
        assert!(parse_hyprctl_clients("{}").is_err());
        assert!(parse_hyprctl_clients("null").is_err());
        assert!(parse_hyprctl_clients("[]").expect("empty array").is_empty());
        assert!(
            parse_hyprctl_clients(r#"[1, "two", null]"#)
                .expect("non-object entries are ignored")
                .is_empty()
        );
        let wrapped = r#"{"clients":[{"address":"0x1","workspace":{"id":2}}]}"#;
        let windows = parse_hyprctl_clients(wrapped).expect("wrapped array");
        assert_eq!(windows[0].desktop, 2);
    }

    #[test]
    fn parse_hyprctl_clients_defaults_missing_fields() {
        let raw = r#"[{"address":"0x1","workspace":{"name":"5"},"pid":"42","title":null,"class":123,"host":"remote"}]"#;
        let windows = parse_hyprctl_clients(raw).expect("valid client JSON");
        let window = &windows[0];
        assert_eq!(window.desktop, 5);
        assert_eq!(window.pid, 42);
        assert!(window.title.is_empty());
        assert_eq!(window.app_class, "123");
        assert_eq!(window.host, "remote");
    }

    #[test]
    fn parsers_clip_long_titles_to_the_python_limit() {
        let long = "x".repeat(MAX_TITLE_CHARS + 40);
        let wmctrl = parse_wmctrl_output(&format!("0x1 0 host {long}\n"));
        assert_eq!(wmctrl[0].title.chars().count(), MAX_TITLE_CHARS);

        let client = format!(r#"[{{"address":"0x2","title":"{long}"}}]"#);
        let hyprctl = parse_hyprctl_clients(&client).expect("valid client JSON");
        assert_eq!(hyprctl[0].title.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn find_matching_window_prefers_exact_id_and_title() {
        let windows = vec![
            sample_window("0xaaa", "Mozilla Firefox", "firefox", 1),
            sample_window("0xbbb", "Terminal", "alacritty", 2),
        ];
        let by_id = find_matching_window(&windows, "0xBBB").expect("id match");
        assert_eq!(by_id.id, "0xbbb");
        let by_title = find_matching_window(&windows, "terminal").expect("title match");
        assert_eq!(by_title.id, "0xbbb");
        let by_class = find_matching_window(&windows, "FIREFOX").expect("class match");
        assert_eq!(by_class.id, "0xaaa");
        let exact_beats_substring =
            find_matching_window(&windows, "Mozilla Firefox").expect("exact title");
        assert_eq!(exact_beats_substring.id, "0xaaa");
    }

    #[test]
    fn find_matching_window_supports_substring_and_fuzzy_matches() {
        let windows = vec![
            sample_window("0x1", "Visual Studio Code", "code", 1),
            sample_window("0x2", "Mozilla Firefox", "firefox", 2),
        ];
        let substring = find_matching_window(&windows, "studio").expect("substring match");
        assert_eq!(substring.id, "0x1");
        let fuzzy = find_matching_window(&windows, "vsc").expect("title subsequence");
        assert_eq!(fuzzy.id, "0x1");
        let fuzzy_class = find_matching_window(&windows, "fx").expect("class subsequence");
        assert_eq!(fuzzy_class.id, "0x2");
        let padded = find_matching_window(&windows, "  VISUAL  ").expect("trimmed query");
        assert_eq!(padded.id, "0x1");
    }

    #[test]
    fn find_matching_window_returns_none_for_empty_or_unknown_queries() {
        let windows = vec![sample_window("0x1", "Terminal", "alacritty", 0)];
        assert!(find_matching_window(&windows, "").is_none());
        assert!(find_matching_window(&windows, "   ").is_none());
        assert!(find_matching_window(&windows, "chromium").is_none());
        assert!(find_matching_window(&[], "term").is_none());
    }

    #[test]
    fn focus_window_live_rejects_invalid_handles_before_spawning() {
        let invalid = ["", "not a handle; rm -rf /", "; echo pwned", "0xZZ", "-"];
        for handle in invalid {
            let error = focus_window_live(handle).expect_err("invalid handle");
            assert!(matches!(&error, NaviError::InvalidInput(_)), "{error:?}");
        }
        assert_eq!(
            normalize_window_id("{0xABCDEF}").expect("braced hex handle"),
            "ABCDEF"
        );
        assert_eq!(
            normalize_window_id("123e4567-e89b-12d3-a456-426614174000").expect("uuid handle"),
            "123e4567-e89b-12d3-a456-426614174000"
        );
    }
}
