//! Harness adapters: discovery, launch specs, native-id capture, interrupt
//! keys, external-session readers, and the shared environment/executable
//! plumbing every profile launch depends on (BUG-069..082).
//!
//! Claude is exact: `--session-id` is documented and the session registry is
//! stable JSON. Codex and Antigravity are best-effort: the code degrades
//! honestly and marks unknowns instead of guessing.

use crate::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const KNOWN_HARNESSES: [&str; 4] = ["claude", "codex", "agy", "pi"];
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(4);
const CAPTURE_POLL: Duration = Duration::from_millis(150);
const VERSION_TIMEOUT: Duration = Duration::from_millis(800);
const SHELL_TIMEOUT: Duration = Duration::from_millis(1500);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_millis(1000);
// Grace period for the output readers once the child is gone. A survivor that
// still holds the write end must not stall the caller.
const DRAIN_GRACE: Duration = Duration::from_millis(200);
// Only the head of a probe's output is ever parsed; the rest is drained so the
// child cannot wedge on a full pipe, but never kept.
const DRAIN_LIMIT: usize = 1024 * 1024;
const WALK_LIMIT: usize = 2000;
const EXTERNAL_ROW_LIMIT: usize = 50;

#[derive(Clone, Copy, Default)]
pub struct Caps {
    /// Harness accepts a caller-chosen conversation id before launch.
    pub preseed_id: bool,
    /// Harness can resume an explicit conversation id or name.
    pub resume_id: bool,
    /// Harness can resume its most recent conversation.
    pub resume_last: bool,
    /// A documented structured prompt channel exists (unused for tmux panes;
    /// bracketed paste remains the transport).
    pub structured_prompt: bool,
    /// Unknown behaviours are marked instead of assumed.
    pub unknowns: &'static [&'static str],
}

impl Caps {
    pub fn json(&self) -> Value {
        json!({
            "preseed_id": self.preseed_id,
            "resume_id": self.resume_id,
            "resume_last": self.resume_last,
            "structured_prompt": self.structured_prompt,
            "unknowns": self.unknowns,
        })
    }
}

/// Canonical adapter id for a profile `harness` field or an executable name.
pub fn adapter_for(harness: &str) -> &'static str {
    match harness {
        "claude" | "claude-code" | "claude_code" => "claude",
        "codex" | "codex-cli" => "codex",
        "agy" | "antigravity" | "antigravity-cli" => "agy",
        other => {
            if other.contains("claude") {
                "claude"
            } else if other.contains("codex") {
                "codex"
            } else if other.contains("agy") || other.contains("antigravity") {
                "agy"
            } else {
                "other"
            }
        }
    }
}

pub fn caps(harness: &str) -> Caps {
    match adapter_for(harness) {
        "claude" => Caps {
            preseed_id: true,
            resume_id: true,
            resume_last: true,
            structured_prompt: false,
            unknowns: &[],
        },
        "codex" => Caps {
            preseed_id: false,
            resume_id: true,
            resume_last: true,
            structured_prompt: true,
            unknowns: &[
                "no documented id preseed; native id scraped from rollout jsonl",
                "structured prompt channel probed, not used over tmux",
            ],
        },
        "agy" => Caps {
            preseed_id: false,
            resume_id: true,
            resume_last: true,
            structured_prompt: true,
            unknowns: &[
                "conversation id approximated from last_conversations.json",
                "structured prompt channel probed, not used over tmux",
            ],
        },
        _ => Caps {
            unknowns: &["generic harness: only the tmux fallback applies"],
            ..Caps::default()
        },
    }
}

#[derive(Clone)]
pub enum Resume {
    New,
    Last,
    Id(String),
}

pub struct LaunchSpec {
    pub argv: Vec<String>,
    pub preseed_id: Option<String>,
    pub native_name: Option<String>,
    pub kind: &'static str,
}

/// Build the harness argv for a launch. `args` are the profile's own arguments
/// and always come last so resume flags stay parseable.
pub fn launch_spec(
    adapter: &str,
    exe: &str,
    args: &[String],
    resume: &Resume,
    name: Option<&str>,
) -> LaunchSpec {
    let mut argv = vec![exe.to_string()];
    let mut preseed_id = None;
    let mut native_name = None;
    let kind;
    match (adapter, resume) {
        ("claude", Resume::New) => {
            kind = "new";
            let generated = uuid();
            argv.push("--session-id".into());
            argv.push(generated.clone());
            preseed_id = Some(generated);
            if let Some(name) = name.filter(|n| !n.is_empty()) {
                argv.push("-n".into());
                argv.push(name.to_string());
                native_name = Some(name.to_string());
            }
        }
        ("claude", Resume::Last) => {
            kind = "resume_last";
            argv.push("--continue".into());
        }
        ("claude", Resume::Id(id)) => {
            kind = "resume";
            argv.push("--resume".into());
            argv.push(id.clone());
        }
        ("codex", Resume::Last) => {
            kind = "resume_last";
            argv.push("resume".into());
            argv.push("--last".into());
        }
        ("codex", Resume::Id(id)) => {
            kind = "resume";
            argv.push("resume".into());
            argv.push(id.clone());
        }
        ("agy", Resume::Last) => {
            kind = "resume_last";
            argv.push("-c".into());
        }
        ("agy", Resume::Id(id)) => {
            kind = "resume";
            argv.push("--conversation".into());
            argv.push(id.clone());
        }
        (_, Resume::Last) => {
            kind = "resume_last";
        }
        (_, Resume::Id(_)) => {
            kind = "resume";
        }
        _ => {
            kind = "new";
        }
    }
    argv.extend(args.iter().cloned());
    LaunchSpec {
        argv,
        preseed_id,
        native_name,
        kind,
    }
}

/// Prompt glyphs an adapter treats as "the harness is ready for input".
pub fn prompt_glyphs(harness: &str) -> &'static [&'static str] {
    match adapter_for(harness) {
        "claude" => &["❯", ">", "│", "╰", "esc to interrupt"],
        "codex" => &["›", ">", "codex>"],
        "agy" => &["❯", ">", "esc to cancel"],
        _ => &["$", "#", ">", "❯", "%", "›"],
    }
}

/// `needs_input` adapter rule: the last visible lines end on a prompt glyph or
/// an interactive confirmation.
pub fn needs_input(harness: &str, tail: &str) -> bool {
    let trimmed = tail.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.ends_with("[Y/n]")
        || trimmed.ends_with("[y/N]")
        || trimmed.ends_with("(y/n)")
        || trimmed.ends_with("(yes/no)")
    {
        return true;
    }
    let Some(last) = trimmed.lines().last() else {
        return false;
    };
    let last = last.trim();
    prompt_glyphs(harness)
        .iter()
        .any(|glyph| last == *glyph || last.ends_with(&format!("{glyph} ")) || last.ends_with(glyph))
}

/// Interrupt keys: soft interrupts a turn, hard interrupts the harness.
pub fn interrupt_keys(harness: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match adapter_for(harness) {
        "claude" | "codex" | "agy" => (&["Escape"], &["C-c"]),
        _ => (&["C-c"], &["C-c"]),
    }
}

// ---------------------------------------------------------------------------
// Executable resolution (BUG-072)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum ExeState {
    Ok,
    NotFound,
    NotExecutable,
    IsDirectory,
}

fn classify(path: &Path) -> ExeState {
    match std::fs::metadata(path) {
        Err(_) => ExeState::NotFound,
        Ok(meta) if meta.is_dir() => ExeState::IsDirectory,
        Ok(meta) => {
            // is_file + an executable bit for *someone*, refined by access(2)
            // for the effective uid.
            if meta.is_file() && meta.mode() & 0o111 != 0 && executable_ok(path) {
                ExeState::Ok
            } else {
                ExeState::NotExecutable
            }
        }
    }
}

fn executable_ok(path: &Path) -> bool {
    let Some(cpath) = CString::new(path.as_os_str().as_bytes()).ok() else {
        return false;
    };
    (unsafe { libc::access(cpath.as_ptr(), libc::X_OK) }) == 0
}

/// Resolve `executable` (absolute or bare) through `launch_dirs`.
/// The error names the failure and the path that was searched.
pub fn resolve(executable: &str, launch_dirs: &[String]) -> Result<String> {
    if executable.is_empty() || executable.starts_with('-') {
        return Err("Invalid executable".into());
    }
    let fail = |state: ExeState, searched: String| -> Result<String> {
        Err(match state {
            ExeState::NotFound => {
                format!("{executable} is not installed or is absent from PATH (searched: {searched})")
            }
            ExeState::NotExecutable => {
                format!("{executable} was found but is not executable (searched: {searched})")
            }
            ExeState::IsDirectory => {
                format!("{executable} resolves to a directory, not an executable (searched: {searched})")
            }
            ExeState::Ok => unreachable!(),
        })
    };
    if executable.contains('/') {
        let path = Path::new(executable);
        match classify(path) {
            ExeState::Ok => {
                return std::fs::canonicalize(path)
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|e| format!("Harness not installed: {e}"));
            }
            state => {
                return fail(state, executable.to_string());
            }
        }
    }
    let searched = launch_dirs.join(":");
    let mut first_problem = ExeState::NotFound;
    for dir in launch_dirs {
        let candidate = Path::new(dir).join(executable);
        match classify(&candidate) {
            ExeState::Ok => {
                return std::fs::canonicalize(&candidate)
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|e| e.to_string());
            }
            state @ (ExeState::IsDirectory | ExeState::NotExecutable) => {
                if first_problem == ExeState::NotFound {
                    first_problem = state;
                }
            }
            ExeState::NotFound => {}
        }
    }
    fail(first_problem, searched)
}

// ---------------------------------------------------------------------------
// Launch PATH and pane environment (BUG-070/072)
// ---------------------------------------------------------------------------

/// The daemon's computed launch PATH as an ordered, deduplicated dir list.
/// Sources, in precedence order: the user's login PATH (per-user dirs first),
/// NAVI_USER_PATH_DEFAULT, ~/.local/bin, then NAVI_SYSTEM_PATH.
pub fn launch_path_dirs() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let system = std::env::var("NAVI_SYSTEM_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    let default_user = std::env::var("NAVI_USER_PATH_DEFAULT").unwrap_or_default();
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/bash".into());
    let login_path = run_timeout(
        Command::new(&shell).args(["-lc", "printf %s \"$PATH\""]),
        SHELL_TIMEOUT,
    )
    .ok()
    .filter(|out| out.status.success())
    .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    .unwrap_or_default();
    let mut dirs: Vec<String> = Vec::new();
    let push = |dirs: &mut Vec<String>, raw: &str| {
        for part in raw.split(':') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let expanded = if part == "~" {
                home.clone()
            } else if let Some(rest) = part.strip_prefix("~/") {
                format!("{home}/{rest}")
            } else {
                part.to_string()
            };
            if !expanded.is_empty() && !dirs.contains(&expanded) {
                dirs.push(expanded);
            }
        }
    };
    // User-owned entries of the login PATH first, then the installer snapshot,
    // then the rest of the login PATH, then the system PATH.
    let user_first: Vec<&str> = login_path
        .split(':')
        .filter(|part| part.starts_with(&home) || part.starts_with('~'))
        .collect();
    for part in &user_first {
        push(&mut dirs, part);
    }
    push(&mut dirs, &default_user);
    push(&mut dirs, &format!("{home}/.local/bin"));
    for part in login_path.split(':') {
        push(&mut dirs, part);
    }
    push(&mut dirs, &system);
    dirs
}

/// Base env for `env -i` launches: the user manager's graphical-session env,
/// then the tmux server global env; explicit vars and profile env sit on top.
pub fn base_env(sock: &Path, managed: bool) -> BTreeMap<String, String> {
    let sock = sock.to_path_buf();
    // systemctl and tmux are independent probes; run them together so the first
    // launch pays one timeout, not the sum of two (BUG-072 launch latency).
    let manager = std::thread::spawn(|| {
        run_timeout(
            Command::new("systemctl").args(["--user", "show-environment"]),
            SYSTEMCTL_TIMEOUT,
        )
    });
    let tmux_handle = std::thread::spawn(move || {
        let mut tmux = Command::new("tmux");
        tmux.arg("-S").arg(&sock);
        if managed {
            tmux.arg("-N");
        }
        run_timeout(tmux.args(["show-environment", "-g"]), SYSTEMCTL_TIMEOUT)
    });
    let mut env = BTreeMap::new();
    if let Ok(Ok(out)) = manager.join()
        && out.status.success()
    {
        parse_env_lines(&out.stdout, &mut env);
    }
    if let Ok(Ok(out)) = tmux_handle.join()
        && out.status.success()
    {
        parse_env_lines(&out.stdout, &mut env);
    }
    // Nothing tmux-adjacent may leak into a pane env.
    env.retain(|key, _| !key.starts_with("TMUX"));
    env
}

fn parse_env_lines(bytes: &[u8], into: &mut BTreeMap<String, String>) {
    for line in String::from_utf8_lossy(bytes).lines() {
        // tmux prefixes removed vars with '-'; both formats use K=V otherwise.
        let line = line.strip_prefix('-').unwrap_or(line);
        if line.is_empty() || !line.contains('=') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if valid_env_key(key) {
            into.insert(key.to_string(), value.to_string());
        }
    }
}

pub fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_uppercase() || c == '_' || (i > 0 && c.is_ascii_digit()))
}

/// Keys whose values must never be logged or stored in argv/events.
pub fn secret_looking(key: &str) -> bool {
    let key = key.to_ascii_uppercase().replace(['-', '.'], "_");
    [
        "TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "AUTH", "CREDENTIAL",
        "PRIVATE", "COOKIE", "SESSION_KEY", "BEARER", "KEYRING", "CERT",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

/// Value-level redaction for argv entries (`--api-key XYZ`, `KEY=XYZ`).
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for (index, arg) in argv.iter().enumerate() {
        if index == 0 {
            out.push(arg.clone());
            continue;
        }
        if redact_next {
            out.push("•••".into());
            redact_next = false;
            continue;
        }
        if let Some((key, _)) = arg
            .trim_start_matches('-')
            .split_once('=')
            .filter(|(key, _)| secret_looking(key))
        {
            let _ = key;
            out.push(format!("{}=•••", arg.split('=').next().unwrap_or_default()));
            continue;
        }
        if secret_looking(arg.trim_start_matches('-')) && arg.len() <= 64 {
            redact_next = true;
        }
        out.push(arg.clone());
    }
    out
}

/// Minimal POSIX shell quoting for the tmux `new-session` shell command.
pub fn sh_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".into();
    }
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Discovery (BUG-071)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Discovery {
    pub found: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub in_navi_path: bool,
}

/// Probe one harness name across the launch PATH, the well-known install
/// dirs, and a login shell `command -v` result.
pub fn discover(name: &str, launch_dirs: &[String], shell_hit: Option<&str>) -> Discovery {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut found_path: Option<PathBuf> = None;
    let mut in_navi_path = false;
    for dir in launch_dirs {
        let candidate = Path::new(dir).join(name);
        if classify(&candidate) == ExeState::Ok {
            found_path = Some(candidate);
            in_navi_path = true;
            break;
        }
    }
    if found_path.is_none() {
        for dir in [
            format!("{home}/.local/bin"),
            "/usr/local/bin".into(),
            format!("{home}/.npm-global/bin"),
            "/usr/bin".into(),
            "/bin".into(),
        ] {
            let candidate = Path::new(&dir).join(name);
            if classify(&candidate) == ExeState::Ok {
                found_path = Some(candidate);
                break;
            }
        }
    }
    if found_path.is_none()
        && let Some(hit) = shell_hit
    {
        let candidate = PathBuf::from(hit);
        if classify(&candidate) == ExeState::Ok {
            in_navi_path = candidate
                .parent()
                .map(|dir| launch_dirs.iter().any(|d| Path::new(d) == dir))
                .unwrap_or(false);
            found_path = Some(candidate);
        }
    }
    let Some(path) = found_path else {
        return Discovery::default();
    };
    let version = run_timeout(
        Command::new(&path)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null()),
        VERSION_TIMEOUT,
    )
    .ok()
    .filter(|out| out.status.success())
    .and_then(|out| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .map(|line| line.chars().take(160).collect())
    });
    Discovery {
        found: true,
        path: Some(path.to_string_lossy().into_owned()),
        version,
        in_navi_path,
    }
}

/// `$SHELL -lc 'command -v …'` for every name in one login shell: each hit is
/// printed as `name<TAB>path`, so a missing name cannot shift the others and
/// only one shell startup (plus its timeout) is paid for the whole set.
pub fn shell_lookup(names: &[&str]) -> BTreeMap<String, String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/bash".into());
    let mut script: String = names
        .iter()
        .map(|name| {
            format!(
                "p=$(command -v {name} 2>/dev/null) && [ -n \"$p\" ] \
                 && printf '%s\\t%s\\n' {name} \"$p\""
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    // The trailing no-op keeps the shell's exit status honest when the last
    // name is absent, so a partial hit list is not mistaken for a failure.
    script.push_str("; :");
    let probe = |program: &str, args: [&str; 2]| {
        run_timeout(Command::new(program).args(args), SHELL_TIMEOUT)
            .ok()
            .filter(|out| out.status.success())
    };
    let out = probe(&shell, ["-lc", &script]).or_else(|| probe("/bin/sh", ["-c", &script]));
    let mut map = BTreeMap::new();
    let Some(out) = out else { return map };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((name, path)) = line.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if path.starts_with('/') {
            map.insert(name.trim().to_string(), path.to_string());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Native-id capture (BUG-069)
// ---------------------------------------------------------------------------

pub type Snapshot = BTreeMap<PathBuf, (u64, u64)>;

/// Snapshot a directory tree (path → (mtime_ns, len)) for store-diff capture.
pub fn snapshot_tree(root: &Path) -> Snapshot {
    let mut map = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        if seen >= WALK_LIMIT {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen >= WALK_LIMIT {
                break;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                map.insert(path, (mtime, meta.len()));
            }
        }
    }
    map
}

/// Directories whose diff reveals a native session id.
pub fn capture_roots(home: &Path, adapter: &str) -> Vec<PathBuf> {
    match adapter {
        "claude" => vec![home.join(".claude/sessions")],
        "codex" => vec![
            home.join(".codex/sessions"),
            home.join(".codex/session_index.jsonl"),
        ],
        "agy" => vec![home.join(".gemini/antigravity-cli/cache")],
        _ => Vec::new(),
    }
}

fn diff_paths(before: &Snapshot, after: &Snapshot) -> Vec<PathBuf> {
    after
        .iter()
        .filter(|(path, meta)| before.get(*path) != Some(meta))
        .map(|(path, _)| path.clone())
        .collect()
}

fn scrape_id(home: &Path, adapter: &str, path: &Path, cwd: &str) -> Option<String> {
    match adapter {
        "claude" => {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let value: Value =
                serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
            let id = value["sessionId"]
                .as_str()
                .or_else(|| value["session_id"].as_str())?
                .to_string();
            Some(id)
        }
        "codex" => {
            if path.file_name().and_then(|n| n.to_str()) == Some("session_index.jsonl") {
                // The index has no cwd; prefer the newest rollout instead.
                return None;
            }
            let text = std::fs::read_to_string(path).ok()?;
            let first = text.lines().next()?;
            let value: Value = serde_json::from_str(first).ok()?;
            let id = value["payload"]["session_id"]
                .as_str()
                .or_else(|| value["session_id"].as_str())?
                .to_string();
            let file_cwd = value["payload"]["cwd"].as_str().unwrap_or_default();
            if !cwd.is_empty() && !file_cwd.is_empty() && file_cwd != cwd {
                return None;
            }
            Some(id)
        }
        "agy" => {
            if path.file_name().and_then(|n| n.to_str()) != Some("last_conversations.json") {
                return None;
            }
            let value: Value =
                serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
            let key = if cwd.is_empty() { "unknown" } else { cwd };
            value
                .get(key)
                .and_then(Value::as_str)
                .or_else(|| {
                    // Approximate: a single-entry map is unambiguous.
                    value.as_object().and_then(|map| {
                        if map.len() == 1 {
                            map.values().next().and_then(Value::as_str)
                        } else {
                            None
                        }
                    })
                })
                .map(str::to_string)
        }
        _ => {
            let _ = (home, path, cwd);
            None
        }
    }
}

/// Poll for the harness's conversation id: preseed wins, registry file next,
/// then a filesystem-store diff. Returns when an id is found or the ≤10 s
/// budget is spent; `None` is honest "unknown", not failure.
pub fn capture_native_id(
    home: &Path,
    adapter: &str,
    cwd: &str,
    pid: Option<u32>,
    preseed: Option<&str>,
    before: &Snapshot,
) -> Option<String> {
    if let Some(id) = preseed.filter(|id| !id.is_empty()) {
        return Some(id.to_string());
    }
    let roots = capture_roots(home, adapter);
    let registry = adapter == "claude" && pid.is_some();
    // A shell has no native store, and a harness home that does not exist yet
    // cannot produce a file: poll only when there is somewhere to look, or the
    // create call stalls for the whole capture budget (BUG-069 launch latency).
    if roots.is_empty() && !registry {
        return None;
    }
    if !registry
        && roots.iter().all(|root| {
            !root.exists() && !root.parent().is_some_and(|parent| parent.exists())
        })
    {
        return None;
    }
    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    loop {
        if adapter == "claude"
            && let Some(pid) = pid
        {
            let registry = home.join(format!(".claude/sessions/{pid}.json"));
            if let Some(id) = scrape_id(home, adapter, &registry, cwd) {
                return Some(id);
            }
        }
        for root in &roots {
            let after = if root.is_dir() {
                snapshot_tree(root)
            } else {
                std::fs::metadata(root)
                    .ok()
                    .map(|meta| {
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0);
                        BTreeMap::from([(root.clone(), (mtime, meta.len()))])
                    })
                    .unwrap_or_default()
            };
            for path in diff_paths(before, &after) {
                if let Some(id) = scrape_id(home, adapter, &path, cwd) {
                    return Some(id);
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(CAPTURE_POLL);
    }
}

// ---------------------------------------------------------------------------
// External sessions (BUG-074)
// ---------------------------------------------------------------------------

/// One deduplicating reader per harness; bounded to 50 rows each. Rows carry
/// `origin:"external"` — callers merge navi rows (which win on native_id).
pub fn external_sessions(home: &Path, adapter: &str) -> Vec<Value> {
    match adapter {
        "claude" => claude_external(home),
        "codex" => codex_external(home),
        "agy" => agy_external(home),
        _ => Vec::new(),
    }
}

fn file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pid_alive(pid: i64) -> bool {
    pid > 1 && unsafe { libc::kill(pid as i32, 0) } == 0
}

fn claude_external(home: &Path) -> Vec<Value> {
    let dir = home.join(".claude/sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort_by_key(|p| std::cmp::Reverse(file_mtime(p)));
    let mut rows = Vec::new();
    for file in files.into_iter().take(EXTERNAL_ROW_LIMIT) {
        let Ok(value) = serde_json::from_str::<Value>(
            &std::fs::read_to_string(&file).unwrap_or_default(),
        ) else {
            continue;
        };
        let Some(native_id) = value["sessionId"]
            .as_str()
            .or_else(|| value["session_id"].as_str())
        else {
            continue;
        };
        let pid = value["pid"].as_i64().unwrap_or(0);
        let alive = pid_alive(pid);
        let status = value["status"].as_str().unwrap_or_default();
        let state = if alive {
            "running"
        } else if status == "running" {
            "exited"
        } else {
            "unknown"
        };
        rows.push(json!({
            "harness": "claude",
            "native_id": native_id,
            "name": value["name"],
            "cwd": value["cwd"],
            "updated": file_mtime(&file),
            "state": state,
            "origin": "external",
        }));
    }
    rows
}

fn codex_external(home: &Path) -> Vec<Value> {
    let index = home.join(".codex/session_index.jsonl");
    let Ok(text) = std::fs::read_to_string(&index) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for line in text.lines().rev() {
        if rows.len() >= EXTERNAL_ROW_LIMIT {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = value["id"]
            .as_str()
            .or_else(|| value["session_id"].as_str())
            .or_else(|| value["sessionId"].as_str());
        let Some(native_id) = id else {
            continue;
        };
        let name = value["thread_name"]
            .as_str()
            .or_else(|| value["name"].as_str())
            .or_else(|| value["title"].as_str());
        let updated = value["updated_at"]
            .as_str()
            .and_then(chrono_like)
            .or_else(|| value["updated_at"].as_u64())
            .or_else(|| value["updated"].as_u64())
            .unwrap_or_else(|| file_mtime(&index));
        rows.push(json!({
            "harness": "codex",
            "native_id": native_id,
            "name": name,
            "cwd": value["cwd"],
            "updated": updated,
            "state": "unknown",
            "origin": "external",
        }));
    }
    // Enrich from rollout files where the index lacks a cwd.
    let mut scanned = 0usize;
    let root = home.join(".codex/sessions");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        if scanned >= 400 || rows.iter().all(|r| r["cwd"].is_string()) {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            scanned += 1;
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(first) = text.lines().next() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(first) else {
                continue;
            };
            let id = value["payload"]["session_id"].as_str().unwrap_or_default();
            let cwd = value["payload"]["cwd"].as_str().unwrap_or_default();
            for row in rows.iter_mut() {
                if row["native_id"] == id && row["cwd"].is_null() && !cwd.is_empty() {
                    row["cwd"] = json!(cwd);
                }
            }
        }
    }
    rows
}

/// Minimal RFC-3339-ish stamp → epoch millis; tolerant of missing pieces.
fn chrono_like(stamp: &str) -> Option<u64> {
    let (date, time) = stamp.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.trim_end_matches('Z').split('+').next()?.split('-').next()?;
    let mut t = time.split(':');
    let (h, min, s): (i64, i64, f64) = (
        t.next()?.parse().ok()?,
        t.next().unwrap_or("0").parse().ok()?,
        t.next().unwrap_or("0").parse().ok()?,
    );
    // Days since epoch (Howard Hinnant's civil-from-days inverse, simplified).
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + min * 60 + s as i64) as u64 * 1000)
}

fn agy_external(home: &Path) -> Vec<Value> {
    let cache = home.join(".gemini/antigravity-cli/cache/last_conversations.json");
    let Ok(value) =
        serde_json::from_str::<Value>(&std::fs::read_to_string(&cache).unwrap_or_default())
    else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let updated = file_mtime(&cache);
    map.iter()
        .take(EXTERNAL_ROW_LIMIT)
        .map(|(cwd, id)| {
            json!({
                "harness": "agy",
                "native_id": id,
                "name": null,
                "cwd": cwd,
                "updated": updated,
                "state": "unknown",
                "origin": "external",
                "approximate": true,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// A v4 UUID from /dev/urandom (no new crates).
pub fn uuid() -> String {
    let mut bytes = [0_u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let h: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let hex: String = h.concat();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Run a command with a deadline. The child leads its own process group, so a
/// timeout kills everything it started: a surviving grandchild would keep the
/// pipes open and block the caller far past the deadline. Output is truncated
/// at the deadline rather than waited for.
pub fn run_timeout(command: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let stdout = Drain::spawn(child.stdout.take());
    let stderr = Drain::spawn(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                terminate(&mut child);
                break child.wait()?;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.collect(),
        stderr: stderr.collect(),
    })
}

/// SIGKILL the child's process group; fall back to the child alone if the
/// group is already gone.
fn terminate(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    // SAFETY: `kill` takes only an integer target and signal; a negative pid
    // names the group whose leader `process_group(0)` made this child.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
}

/// One pipe reader filling a shared buffer, so the caller can take the bytes
/// read so far instead of blocking on EOF.
struct Drain {
    handle: Option<std::thread::JoinHandle<()>>,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Drain {
    fn spawn(pipe: Option<impl Read + Send + 'static>) -> Self {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        let handle = pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                while let Ok(read) = pipe.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    let mut sink = sink.lock().unwrap();
                    let room = DRAIN_LIMIT.saturating_sub(sink.len());
                    sink.extend_from_slice(&chunk[..read.min(room)]);
                }
            })
        });
        Self { handle, buffer }
    }

    /// Wait a short grace period for EOF, then take whatever arrived.
    fn collect(self) -> Vec<u8> {
        if let Some(handle) = self.handle {
            let deadline = Instant::now() + DRAIN_GRACE;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        std::mem::take(&mut self.buffer.lock().unwrap())
    }
}

/// Strip ANSI/VT sequences for `session_output` and screen tails.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    // CSI: consume until a byte in 0x40..=0x7e.
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: ends at BEL or ST (ESC \).
                    let mut prev = '\0';
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (prev == '\u{1b}' && next == '\\') {
                            break;
                        }
                        prev = next;
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        if c != '\u{9b}' && (c >= ' ' || c == '\n' || c == '\t' || c == '\r') {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_v4_shaped() {
        let id = uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!("89ab".contains(&id[19..20]));
    }

    #[test]
    fn redaction_hides_secret_values() {
        let argv = vec![
            "claude".to_string(),
            "--api-key".to_string(),
            "sk-live".to_string(),
            "NORMAL=1".to_string(),
            "GH_TOKEN=abc".to_string(),
        ];
        let redacted = redact_argv(&argv);
        assert_eq!(redacted[2], "•••");
        assert_eq!(redacted[4], "GH_TOKEN=•••");
        assert_eq!(redacted[3], "NORMAL=1");
    }

    #[test]
    fn ansi_sequences_are_stripped() {
        assert_eq!(strip_ansi("a\u{1b}[31mb\u{1b}[0mc"), "abc");
        assert_eq!(strip_ansi("x\u{1b}]0;title\u{7}y"), "xy");
    }

    #[test]
    fn quoting_round_trips_spaces() {
        assert_eq!(sh_quote("plain"), "plain");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
