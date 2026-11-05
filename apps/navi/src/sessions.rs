//! Harness session lifecycle: tmux-backed panes with profile v2 launches,
//! native-id capture, pipe logging, readiness-aware prompt injection, resume,
//! external discovery, and honest state (BUG-069..082).

use crate::{App, Result, adapters, id, link, now, store};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INJECT_LIMIT: usize = 32 * 1024;
const INJECT_ENTER_DELAY: Duration = Duration::from_millis(120);
const READY_TIMEOUT: Duration = Duration::from_secs(8);
const READY_POLL: Duration = Duration::from_millis(100);
const LAUNCH_SETTLE: Duration = Duration::from_millis(1200);
const LOG_LIMIT: u64 = 8 * 1024 * 1024;
const LOG_KEEP: u8 = 3;
const TAIL_BYTES: u64 = 1024;
const TAIL_LINES: usize = 12;
const STATE_EMIT_INTERVAL: u64 = 1000;
const DISCOVERY_TTL: u64 = 60_000;
const EXTERNAL_TTL: u64 = 5_000;
const PATH_TTL: u64 = 60_000;
const HARD_KILL_AFTER: Duration = Duration::from_secs(3);
/// Pane history read by `harness_tail`; the plan asks for 200 lines.
const TAIL_LINES_DEFAULT: i64 = 200;
const TAIL_LINES_MAX: i64 = 5_000;
/// Revision of the parse rules below (BUG-087). A rule change bumps this, so a
/// signal read under older rules can be told apart from a newer read of the
/// same pane instead of silently becoming the same evidence.
const TAIL_RULES_VERSION: u32 = 1;
/// Post-prompt sampling: once after 5 s, then every 10 s for 90 s (BUG-087).
const TAIL_SAMPLE_AFTER_PROMPT: Duration = Duration::from_secs(5);
const TAIL_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
const TAIL_SAMPLE_WINDOW: Duration = Duration::from_secs(90);
const SCREENSHOT_MARKER: &str = "\0SCREENSHOT";
const SHELL_COMMANDS: [&str; 6] = ["bash", "sh", "zsh", "fish", "dash", "tmux"];

#[derive(Clone, Default)]
struct Row {
    id: String,
    name: String,
    profile: String,
    cwd: String,
    harness: String,
    native_id: Option<String>,
    native_name: Option<String>,
    state: String,
    pid: Option<u32>,
    generation: i64,
    origin: String,
    exit_status: Option<i64>,
    last_active: Option<u64>,
    exited: Option<u64>,
    project_id: Option<String>,
    orphan: bool,
    replaced_by: Option<String>,
    launch_kind: Option<String>,
}

#[derive(Clone, Default, Debug)]
struct Pane {
    session: String,
    dead: bool,
    dead_status: Option<i64>,
    command: String,
    pid: Option<u32>,
}

/// Post-prompt tail sampling schedule for one session (BUG-087). One plan is
/// live at a time: the most recent prompt owns the schedule, so an idle attach
/// can never resurrect a stale session.
#[derive(Clone)]
pub(crate) struct TailPlan {
    session: String,
    next_at: u64,
    until: u64,
    interval: Duration,
}

/// The task a tail is attributed to, with enough of its row to report state.
struct TailTask {
    id: String,
    aspect: Option<String>,
    state: String,
}

/// (first sample, interval, window). `NAVI_TAIL_SAMPLE_MS` compresses all three
/// so a test can prove the sampler without waiting the production 90 s.
fn tail_schedule() -> (Duration, Duration, Duration) {
    match std::env::var("NAVI_TAIL_SAMPLE_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
    {
        Some(millis) => (
            Duration::from_millis(millis),
            Duration::from_millis(millis),
            Duration::from_millis(millis.saturating_mul(6)),
        ),
        None => (
            TAIL_SAMPLE_AFTER_PROMPT,
            TAIL_SAMPLE_INTERVAL,
            TAIL_SAMPLE_WINDOW,
        ),
    }
}

/// One decisive test line out of a captured pane (BUG-087).
#[derive(Clone, PartialEq, Debug)]
struct TailVerdict {
    pass: bool,
    /// The exact line, newest-last: this text is the signal's evidence.
    line: String,
    /// How many tests the line claims, when it says so.
    count: Option<u32>,
    /// The rule family whose table read this pane.
    family: String,
    rules: u32,
}

/// Which rule table reads a profile's pane. A known harness uses its adapter id;
/// anything else (the terminal profile, a custom wrapper) is its own family, so
/// the revision reported with a signal names the profile it came from.
fn tail_family(harness: &str) -> String {
    let adapter = adapters::adapter_for(harness);
    if adapter != "other" {
        return adapter.to_string();
    }
    let trimmed = harness.trim();
    if trimmed.is_empty() {
        "terminal".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

/// Read the newest decisive line out of a pane. `None` means the output is
/// ambiguous — no signal is written rather than a guess (BUG-087).
fn parse_tail(harness: &str, text: &str) -> Option<TailVerdict> {
    let family = tail_family(harness);
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() || line.len() > 400 {
            continue;
        }
        if let Some(verdict) = classify_tail_line(&family, line) {
            return Some(verdict);
        }
    }
    None
}

/// Per-profile parse rules, versioned by [`TAIL_RULES_VERSION`]. A harness that
/// prints a test report in the common shapes (`PASSED 12`, `12 passed`,
/// `3 failed`, `test result: ok. 12 passed; 0 failed`) is read here; anything
/// else falls through and produces no signal.
fn classify_tail_line(family: &str, line: &str) -> Option<TailVerdict> {
    let lower = line.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    // A failure count outranks any pass on the same line: "3 passed, 1 failed"
    // is a failing run, and optimism is the failure mode this rule exists for.
    for (index, token) in tokens.iter().enumerate() {
        if matches!(*token, "failed" | "failing" | "failure" | "failures")
            && adjacent_count(&tokens, index) != Some(0)
        {
            return Some(TailVerdict {
                pass: false,
                line: line.to_string(),
                count: adjacent_count(&tokens, index),
                family: family.to_string(),
                rules: TAIL_RULES_VERSION,
            });
        }
    }
    for (index, token) in tokens.iter().enumerate() {
        if matches!(*token, "passed" | "passing" | "pass")
            && adjacent_count(&tokens, index) != Some(0)
        {
            return Some(TailVerdict {
                pass: true,
                line: line.to_string(),
                count: adjacent_count(&tokens, index),
                family: family.to_string(),
                rules: TAIL_RULES_VERSION,
            });
        }
    }
    None
}

/// The number next to a keyword: the token before it wins, else the one after.
/// `12 passed` and `PASSED 12` both read twelve; `0 failed` reads zero, which
/// the caller treats as "this line is not a failure".
fn adjacent_count(tokens: &[&str], index: usize) -> Option<u32> {
    let parse = |token: &str| -> Option<u32> {
        token.parse::<u32>().ok().filter(|count| *count <= 1_000_000)
    };
    if index > 0
        && let Some(count) = parse(tokens[index - 1])
    {
        return Some(count);
    }
    tokens.get(index + 1).and_then(|token| parse(token))
}

/// The last `limit` bytes of a pane, cut on a character boundary. The tail RPC
/// returns the captured text so a caller can see what was read; the store keeps
/// the whole screen for evidence.
fn clip_tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut start = text.len() - limit;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// `navi:task=<id>` trailers anywhere in the output, in read order. Attribution
/// comes from this trailer or an explicit request parameter, never from prose.
fn task_hints(text: &str) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(index) = rest.find("navi:task=") {
            rest = &rest[index + "navi:task=".len()..];
            let end = rest
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, ',' | ';' | ')' | ']' | '"')
                })
                .unwrap_or(rest.len());
            let hint = rest[..end]
                .trim_matches(|character: char| matches!(character, '\'' | '"' | '.' | ':'))
                .to_string();
            if !hint.is_empty() && !hints.contains(&hint) {
                hints.push(hint);
            }
        }
    }
    hints
}

/// One second of shared tmux truth for every status caller. The UI polls the
/// daemon at 700 ms and the host pushes at 40 ms while a turn is open, so a fork
/// per call per session is the difference between an idle app and a busy one
/// (BUG-094).
const STATUS_TTL: u64 = 1_000;
/// Bounded so a long-lived daemon cannot accumulate a screen per dead session.
const STATUS_CACHE_LIMIT: usize = 64;

/// `list-panes` and the visible tail of each session, cached for
/// [`STATUS_TTL`] and shared: the reconciler, the session listing, and the
/// voice target tail all read the same snapshot.
pub struct StatusCache {
    panes: Mutex<Option<(u64, Vec<Pane>)>>,
    screens: Mutex<HashMap<(String, i64), (u64, String)>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for StatusCache {
    fn default() -> Self {
        Self {
            panes: Mutex::new(None),
            screens: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }
}

impl StatusCache {
    /// Diagnostics for `status.cache`: how old the shared snapshot is and how
    /// often it answered without forking.
    pub(crate) fn snapshot(&self) -> Value {
        let panes = self.panes.lock().unwrap();
        let (age_ms, sessions) = match panes.as_ref() {
            Some((stamp, panes)) => (
                json!(now().saturating_sub(*stamp)),
                json!(panes.len()),
            ),
            None => (Value::Null, json!(0)),
        };
        drop(panes);
        json!({
            "ttl_ms":STATUS_TTL,
            "panes":{"age_ms":age_ms,"sessions":sessions,
                "hits":self.hits.load(Ordering::Relaxed),
                "misses":self.misses.load(Ordering::Relaxed)},
            "screens":self.screens.lock().unwrap().len(),
        })
    }
}

impl App {
    pub fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.arg("-f").arg(self.runtime.join("tmux.conf"));
        command.arg("-S").arg(self.data.join("terminals.sock"));
        if std::env::var("NAVI_MANAGED_TERMINALS").as_deref() == Ok("1") {
            command.arg("-N");
        }
        command
    }

    fn tmux_out(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut command = self.tmux();
        command.args(args);
        command
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .map_err(|e| e.to_string())
    }

    fn tmux_ok(&self, args: &[&str]) -> bool {
        // Starting the server is the one command whose child can outlive the
        // client (exit-empty off). If it inherited our captured stdout pipe,
        // output() would wait for EOF that never comes, so detach its stdio.
        let mut command = self.tmux();
        command
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// One `list-panes` call feeds liveness, pid, exit status, and command —
    /// the only tmux probe the reconciler needs. The answer is shared for
    /// [`STATUS_TTL`], so a burst of status calls forks tmux at most once.
    fn panes(&self) -> Vec<Pane> {
        if let Some((stamp, panes)) = self.status_cache.panes.lock().unwrap().as_ref()
            && now().saturating_sub(*stamp) < STATUS_TTL
        {
            self.status_cache.hits.fetch_add(1, Ordering::Relaxed);
            return panes.clone();
        }
        self.status_cache.misses.fetch_add(1, Ordering::Relaxed);
        let panes = self.panes_uncached();
        *self.status_cache.panes.lock().unwrap() = Some((now(), panes.clone()));
        panes
    }

    fn panes_uncached(&self) -> Vec<Pane> {
        // tmux 3.4 prints control characters as octal text (`\x1f` comes back
        // as `\037`), so the separator must be printable. tmux forbids `:` in
        // session names and the numeric fields cannot hold one; the free-form
        // command goes last so a `:` inside it stays in that field.
        let Ok(output) = self.tmux_out(&[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}:#{pane_dead}:#{pane_dead_status}:#{pane_pid}:#{pane_current_command}",
        ]) else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(5, ':');
                Some(Pane {
                    session: parts.next()?.to_string(),
                    dead: parts.next()? == "1",
                    dead_status: parts.next().and_then(|s| s.parse().ok()),
                    pid: parts.next().and_then(|s| s.parse().ok()),
                    command: parts.next().unwrap_or_default().to_string(),
                })
            })
            .collect()
    }

    /// The visible screen, shared for [`STATUS_TTL`]: state derivation and the
    /// voice tail read it on every status poll, and the answer cannot change
    /// meaningfully inside one second (BUG-094).
    fn cached_screen(&self, target: &str, history: i64) -> String {
        let key = (target.to_string(), history);
        if let Some((stamp, screen)) = self.status_cache.screens.lock().unwrap().get(&key)
            && now().saturating_sub(*stamp) < STATUS_TTL
        {
            return screen.clone();
        }
        let screen = self.capture_pane(target, history);
        let mut screens = self.status_cache.screens.lock().unwrap();
        if screens.len() >= STATUS_CACHE_LIMIT && !screens.contains_key(&key) {
            screens.clear(); // dead sessions must not pin memory forever
        }
        screens.insert(key, (now(), screen.clone()));
        screen
    }

    fn capture_pane(&self, target: &str, history: i64) -> String {
        self.tmux_out(&[
            "capture-pane",
            "-p",
            "-J",
            "-S",
            &format!("-{history}"),
            "-t",
            target,
        ])
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
    }

    fn session_log(&self, target: &str) -> PathBuf {
        self.data.join("sessions").join(format!("{target}.log"))
    }

    fn open_log(&self, target: &str) -> Result<fs::File> {
        fs::create_dir_all(self.data.join("sessions")).map_err(|e| e.to_string())?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.session_log(target))
            .map_err(|e| e.to_string())
    }

    /// `8 MiB × 3` rotation: `<id>.log` → `.1`, `.1` → `.2`, `.2` → `.3`.
    fn maybe_rotate(&self, target: &str) {
        let log = self.session_log(target);
        let Ok(meta) = fs::metadata(&log) else {
            return;
        };
        if meta.len() < LOG_LIMIT {
            return;
        }
        for n in (1..LOG_KEEP).rev() {
            let from = self
                .data
                .join("sessions")
                .join(format!("{target}.log.{n}"));
            let to = self
                .data
                .join("sessions")
                .join(format!("{target}.log.{}", n + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        let _ = fs::rename(&log, self.data.join("sessions").join(format!("{target}.log.1")));
        // New pipe output recreates the log via append-mode open.
    }

    fn append_screen(&self, target: &str) -> Result<()> {
        let screen = self.capture_pane(target, 400);
        let mut file = self.open_log(target)?;
        file.write_all(format!("{SCREENSHOT_MARKER}\n{screen}").as_bytes())
            .map_err(|e| e.to_string())
    }

    // ------------------------------------------------------------------
    // Rows and reconciliation
    // ------------------------------------------------------------------

    fn rows(&self, include_closed: bool) -> Result<Vec<Row>> {
        let state = self.state.lock().unwrap();
        let sql = if include_closed {
            "SELECT id,name,profile,cwd,harness,native_id,native_name,state,pid,gen,origin,\
                    exit_status,last_active,exited,project_id,orphan,replaced_by,launch_kind \
             FROM sessions ORDER BY created"
        } else {
            "SELECT id,name,profile,cwd,harness,native_id,native_name,state,pid,gen,origin,\
                    exit_status,last_active,exited,project_id,orphan,replaced_by,launch_kind \
             FROM sessions WHERE closed_at IS NULL ORDER BY created"
        };
        let mut query = state.db.prepare(sql).map_err(|e| e.to_string())?;
        let rows = query
            .query_map([], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    profile: row.get(2)?,
                    cwd: row.get(3)?,
                    harness: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    native_id: row.get(5)?,
                    native_name: row.get(6)?,
                    state: row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "created".into()),
                    pid: row.get::<_, Option<i64>>(8)?.map(|p| p as u32),
                    generation: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    origin: row.get::<_, Option<String>>(10)?.unwrap_or_else(|| "navi".into()),
                    exit_status: row.get(11)?,
                    last_active: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                    exited: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                    project_id: row.get(14)?,
                    orphan: row.get::<_, Option<i64>>(15)?.unwrap_or(0) != 0,
                    replaced_by: row.get(16)?,
                    launch_kind: row.get(17)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// Reconcile DB rows with real tmux panes; called at boot and from every
    /// session listing so dead harnesses are captured once, not hidden.
    pub fn reconcile_sessions(&self) -> Result<()> {
        self.reconcile_rows(false).map(|_| ())
    }

    fn reconcile_rows(&self, include_closed: bool) -> Result<Vec<Row>> {
        let panes: BTreeMap<String, Pane> = self
            .panes()
            .into_iter()
            .map(|pane| (pane.session.clone(), pane))
            .collect();
        let rows = self.rows(include_closed)?;
        for row in &rows {
            if row.state == "removed" {
                continue;
            }
            let pane = panes.get(&row.id);
            let derived = match pane {
                None => "exited".to_string(),
                Some(pane) if pane.dead => "exited".to_string(),
                Some(pane) => self.derive_state(row, pane),
            };
            if derived == "exited" && row.state != "exited" {
                // First observation of the death: keep the last screen.
                if panes.contains_key(&row.id) {
                    let _ = self.append_screen(&row.id);
                }
                let status = pane.and_then(|p| p.dead_status).or(row.exit_status);
                self.mark_session(&row.id, "exited", status);
                {
                    let state = self.state.lock().unwrap();
                    let _ = state.db.execute(
                        "UPDATE sessions SET exited=?1 WHERE id=?2 AND exited IS NULL",
                        params![now() as i64, row.id],
                    );
                }
                self.log_event(&row.id, "exited", json!({"exit_status":status}));
                self.event_for(
                    row.project_id.as_deref(),
                    Some(&row.id),
                    "session_state",
                    "Harness session exited",
                    json!({"id":row.id,"state":"exited","exit_status":status}),
                );
            } else if derived != "exited" {
                let changed = derived != row.state
                    || pane.and_then(|p| p.pid) != row.pid
                    || row.state == "starting";
                if changed {
                    let state = self.state.lock().unwrap();
                    let _ = state.db.execute(
                        "UPDATE sessions SET state=?1,last_active=?2,pid=?3 WHERE id=?4",
                        params![
                            derived,
                            now() as i64,
                            pane.and_then(|p| p.pid).map(|p| p as i64),
                            row.id
                        ],
                    );
                }
                self.emit_transition(row, &derived);
            }
            self.maybe_rotate(&row.id);
        }
        self.rows(include_closed)
    }

    /// Adapter-owned state derivation: the pane's process and (cheaply, only
    /// when the launcher itself is at the foreground) the visible tail.
    fn derive_state(&self, row: &Row, pane: &Pane) -> String {
        if pane.dead {
            return "exited".into();
        }
        if row.state == "starting" || row.state == "created" {
            return "running".into();
        }
        let launcher = self.launcher_command(row);
        // A plain terminal sitting at its own prompt is idle, not asking for
        // anything; only a harness prompt means "needs input".
        if pane.command != launcher || SHELL_COMMANDS.contains(&launcher.as_str()) {
            if SHELL_COMMANDS.contains(&pane.command.as_str()) {
                return "idle".into();
            }
            return "busy".into();
        }
        let screen = self.cached_screen(&row.id, 12);
        if adapters::needs_input(&row.harness, &screen) {
            "needs_input".into()
        } else {
            "running".into()
        }
    }

    /// The command the pane was forked with (the executable basename), used to
    /// tell "at the harness prompt" from "a child is running".
    fn launcher_command(&self, row: &Row) -> String {
        let state = self.state.lock().unwrap();
        let argv: Option<String> = state
            .db
            .query_row(
                "SELECT argv FROM sessions WHERE id=?1",
                [&row.id],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        argv.and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.as_array().cloned())
            .and_then(|a| a.first().and_then(Value::as_str).map(str::to_string))
            .and_then(|exe| {
                Path::new(&exe)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| row.harness.clone())
    }

    /// ≤1/s per session transition events into the real event stream plus the
    /// durable session ledger.
    fn emit_transition(&self, row: &Row, derived: &str) {
        if derived == row.state {
            return;
        }
        {
            let mut marks = self.state_marks.lock().unwrap();
            let last = marks
                .get(&(row.id.clone(), derived.to_string()))
                .copied()
                .unwrap_or(0);
            if now().saturating_sub(last) < STATE_EMIT_INTERVAL {
                return;
            }
            marks.insert((row.id.clone(), derived.to_string()), now());
        }
        let kind = match derived {
            "running" => "ready",
            "idle" => "idle",
            "busy" => "busy",
            "needs_input" => "needs_input",
            _ => "state",
        };
        self.log_event(&row.id, kind, json!({"state":derived}));
        if matches!(derived, "needs_input" | "busy" | "idle") {
            self.event_for(
                row.project_id.as_deref(),
                Some(&row.id),
                "session_state",
                "Harness session state changed",
                json!({"id":row.id,"state":derived}),
            );
        }
    }

    fn mark_session(&self, target: &str, state: &str, exit_status: Option<i64>) {
        let state_db = self.state.lock().unwrap();
        let _ = state_db.db.execute(
            "UPDATE sessions SET state=?1,last_active=?2,exit_status=?3 WHERE id=?4",
            params![state, now() as i64, exit_status, target],
        );
        let _ = state_db.db.execute(
            "INSERT INTO session_events(session,time,kind,payload) VALUES(?1,?2,'state',?3)",
            params![
                target,
                now() as i64,
                json!({"state":state,"exit_status":exit_status}).to_string()
            ],
        );
    }

    fn log_event(&self, target: &str, kind: &str, payload: Value) {
        let state = self.state.lock().unwrap();
        let _ = state.db.execute(
            "INSERT INTO session_events(session,time,kind,payload) VALUES(?1,?2,?3,?4)",
            params![target, now() as i64, kind, payload.to_string()],
        );
    }

    // ------------------------------------------------------------------
    // Listings
    // ------------------------------------------------------------------

    pub fn sessions(&self) -> Result<Vec<Value>> {
        let rows = self.reconcile_rows(false)?;
        let panes: BTreeSet<String> = self
            .panes()
            .into_iter()
            .map(|pane| pane.session)
            .collect();
        let settings = self.settings();
        let mut records = Vec::new();
        for row in rows {
            let running = panes.contains(&row.id);
            let mut state_name = row.state.clone();
            if !running && state_name != "exited" {
                state_name = "exited".into();
            }
            records.push(json!({
                "id": row.id,
                "name": row.name,
                "profile": row.profile,
                "cwd": row.cwd,
                "created": row.last_active,
                "harness": row.harness,
                "native_id": row.native_id,
                "native_name": row.native_name,
                "state": state_name,
                "project_id": row.project_id,
                "origin": row.origin,
                "exit_status": row.exit_status,
                "last_active": row.last_active,
                "running": running,
                "gen": row.generation,
                "launch_kind": row.launch_kind,
                "orphan": row.orphan,
                "replaced_by": row.replaced_by,
                "pid": row.pid,
                "exited": row.exited,
                "active": settings["active_session"].as_str() == Some(row.id.as_str()),
                "voice_target": settings["active_session"].as_str() == Some(row.id.as_str()),
                "last_launched": settings["last_launched"].as_str() == Some(row.id.as_str()),
            }));
        }
        // `created` was left off the row struct; query kept it out to keep the
        // struct slim — restore it for the UI ordering contract.
        let state = self.state.lock().unwrap();
        for record in &mut records {
            if let Some(id) = record["id"].as_str() {
                let created: i64 = state
                    .db
                    .query_row(
                        "SELECT created FROM sessions WHERE id=?1",
                        [id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                record["created"] = json!(created);
            }
        }
        Ok(records)
    }

    pub fn require_session(&self, target: &str) -> Result<()> {
        if self
            .sessions()?
            .iter()
            .any(|s| s["id"] == target && s["running"] == true)
        {
            Ok(())
        } else {
            Err("Session is not running".into())
        }
    }

    /// `active_session_tail`: bounded view of the voice-targeted pane so the
    /// status answer "what is Claude asking?" is a read, not an attach.
    pub fn active_session_tail(&self) -> Value {
        let settings = self.settings();
        let Some(target) = settings["active_session"].as_str() else {
            return Value::Null;
        };
        let screen = self.cached_screen(target, TAIL_LINES as i64);
        let stripped = adapters::strip_ansi(&screen);
        let mut lines: Vec<&str> = stripped
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if lines.len() > TAIL_LINES {
            lines = lines[lines.len() - TAIL_LINES..].to_vec();
        }
        let mut tail = lines.join("\n");
        if tail.len() > TAIL_BYTES as usize {
            tail = tail[tail.len() - TAIL_BYTES as usize..].to_string();
        }
        json!({"id":target,"tail":tail})
    }

    // ------------------------------------------------------------------
    // Launch (create / resume / restart share one path)
    // ------------------------------------------------------------------

    fn profile(&self, id: &str) -> Result<Value> {
        self.settings()["profiles"]
            .as_array()
            .ok_or("Missing profiles")?
            .iter()
            .find(|profile| profile["id"] == id)
            .cloned()
            .ok_or_else(|| "Unknown installed launch profile".into())
    }

    /// Computed launch PATH dirs, cached 60 s (BUG-072).
    fn launch_dirs(&self) -> Vec<String> {
        let mut cache = self.launch_path_cache.lock().unwrap();
        if let Some((stamp, dirs)) = cache.as_ref()
            && now().saturating_sub(*stamp) < PATH_TTL
        {
            return dirs.clone();
        }
        let dirs = adapters::launch_path_dirs();
        *cache = Some((now(), dirs.clone()));
        dirs
    }

    /// Warm the launch environment caches off the request path (BUG-072).
    pub(crate) fn warm_launch(&self) {
        let _ = self.launch_dirs();
        let _ = self.base_env();
    }

    /// Base pane env (user manager + tmux global), cached 60 s; the refresh is
    /// explicit on daemon start and settings save per BUG-070.
    fn base_env(&self) -> BTreeMap<String, String> {
        let mut cache = self.base_env_cache.lock().unwrap();
        if let Some((stamp, env)) = cache.as_ref()
            && now().saturating_sub(*stamp) < PATH_TTL
        {
            return env.clone();
        }
        let managed = std::env::var("NAVI_MANAGED_TERMINALS").as_deref() == Ok("1");
        let env = adapters::base_env(&self.data.join("terminals.sock"), managed);
        *cache = Some((now(), env.clone()));
        env
    }

    /// The `env -i … exe args` shell command tmux runs in the pane.
    fn launch_command(&self, argv: &[String], env: &BTreeMap<String, String>) -> String {
        let mut parts = vec!["env".to_string(), "-i".to_string()];
        for (key, value) in env {
            parts.push(format!("{key}={}", adapters::sh_quote(value)));
        }
        for arg in argv {
            parts.push(adapters::sh_quote(arg));
        }
        parts.join(" ")
    }

    fn compose_env(
        &self,
        profile_env: &Value,
        launch_path: &str,
    ) -> Result<BTreeMap<String, String>> {
        let mut env = self.base_env();
        env.insert(
            "HOME".into(),
            std::env::var("HOME").map_err(|e| e.to_string())?,
        );
        env.insert("PATH".into(), launch_path.to_string());
        env.insert("TERM".into(), "tmux-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            env.insert("XDG_RUNTIME_DIR".into(), xdg);
        } else {
            let candidate = format!("/run/user/{}", unsafe { libc::geteuid() });
            if Path::new(&candidate).is_dir() {
                env.insert("XDG_RUNTIME_DIR".into(), candidate);
            }
        }
        if let Some(map) = profile_env.as_object() {
            for (key, value) in map {
                let value = value.as_str().unwrap_or_default();
                env.insert(key.clone(), value.to_string());
            }
        }
        env.retain(|key, _| !key.starts_with("TMUX"));
        Ok(env)
    }

    /// Shared launch for `create`, `session_resume`, and `session_restart`.
    /// Returns the created row payload; `launch` errors leave an `exited` row
    /// only when the row was already written (BUG-080 contract).
    #[allow(clippy::too_many_arguments)]
    fn launch_managed(
        &self,
        profile: &Value,
        cwd: &str,
        name: &str,
        resume: adapters::Resume,
        native_hint: Option<String>,
        project_override: Option<Option<String>>,
        prompt: Option<&str>,
        generation: i64,
    ) -> Result<Value> {
        let profile_id = profile["id"].as_str().unwrap_or_default().to_string();
        let executable = profile["executable"].as_str().ok_or("Missing executable")?;
        let args: Vec<String> = profile["args"]
            .as_array()
            .map(|args| {
                args.iter()
                    .filter_map(|arg| arg.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let harness = profile["harness"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| {
                Path::new(executable)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| "shell".into())
            });
        let adapter = adapters::adapter_for(&harness);
        // Resolve through the computed launch PATH with named failures so a
        // non-executable or absent harness errors before any row exists.
        let dirs = self.launch_dirs();
        let resolved = adapters::resolve(executable, &dirs)?;
        let spec = adapters::launch_spec(adapter, &resolved, &args, &resume, Some(name));
        // Codex resume pins the working directory explicitly.
        let mut argv = spec.argv.clone();
        if adapter == "codex" && !matches!(resume, adapters::Resume::New) {
            argv.insert(1, "-C".to_string());
            argv.insert(2, cwd.to_string());
        }
        let path = std::fs::canonicalize(cwd)
            .map_err(|e| format!("Invalid project directory: {e}"))?;
        if !path.is_dir() {
            return Err("Project path must be a directory".into());
        }
        let path_text = path.to_string_lossy().into_owned();
        let project_id = match project_override {
            Some(explicit) => explicit,
            None => {
                let state = self.state.lock().unwrap();
                store::project_id_for_path(&state.db, &path_text)
            }
        };
        let target = format!("navi-{}", id());
        let launch_path = dirs.join(":");
        let mut env = self.compose_env(&profile["env"], &launch_path)?;
        // The progress link (see link.rs): every pane knows its project and
        // session, and harnesses that can are registered with `navid mcp`.
        // A profile opts out with `"link": false`.
        if profile["link"] != false
            && let Ok(exe) = link::navid_path()
        {
            let mut link_env = BTreeMap::new();
            link_env.insert("NAVI_SESSION_ID".to_string(), target.clone());
            link_env.insert("NAVI_HARNESS".to_string(), adapter.to_string());
            link_env.insert("NAVI_DATA_DIR".to_string(), self.data.to_string_lossy().into_owned());
            link_env.insert(
                "NAVI_RUNTIME_DIR".to_string(),
                self.runtime.to_string_lossy().into_owned(),
            );
            link_env.insert("NAVI_BIN".to_string(), exe.to_string_lossy().into_owned());
            if let Some(project) = project_id.as_deref() {
                link_env.insert("NAVI_PROJECT_ID".to_string(), project.to_string());
            }
            let registration = link::registration(adapter, &exe, &link_env, &env);
            for (offset, arg) in registration.argv.into_iter().enumerate() {
                argv.insert(1 + offset, arg);
            }
            // `navid progress` must resolve inside the pane (Pi calls it).
            let current = env.get("PATH").cloned().unwrap_or_default();
            if let Some(dir) = exe.parent().map(|dir| dir.to_string_lossy().into_owned())
                && !current.split(':').any(|entry| entry == dir)
            {
                env.insert("PATH".to_string(), format!("{current}:{dir}"));
            }
            env.extend(link_env);
            env.extend(registration.env);
        }
        let env_keys: Vec<String> = env.keys().cloned().collect();
        let cols = profile["size"]["cols"].as_u64().unwrap_or(120).clamp(20, 500);
        let rows_size = profile["size"]["rows"].as_u64().unwrap_or(32).clamp(8, 200);
        let native_id = spec.preseed_id.clone().or(native_hint);
        let snapshot = if adapters::capture_roots(Path::new(
            &std::env::var("HOME").unwrap_or_default(),
        ), adapter)
        .is_empty()
        {
            adapters::Snapshot::new()
        } else {
            let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
            let mut snapshot = adapters::Snapshot::new();
            for root in adapters::capture_roots(&home, adapter) {
                if root.is_dir() {
                    snapshot.extend(adapters::snapshot_tree(&root));
                } else if let Ok(meta) = fs::metadata(&root) {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    snapshot.insert(root, (mtime, meta.len()));
                }
            }
            snapshot
        };
        let launch_kind = spec.kind;
        let stored_argv = adapters::redact_argv(&argv);
        {
            let state = self.state.lock().unwrap();
            state
                .db
                .execute(
                    "INSERT INTO sessions(id,name,profile,cwd,created,harness,native_id,native_name,\
                     argv,env_keys,launch_kind,state,origin,gen,project_id,last_active) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'starting','navi',?12,?13,?14)",
                    params![
                        target,
                        name,
                        profile_id,
                        path_text,
                        now() as i64,
                        harness,
                        native_id,
                        spec.native_name.clone(),
                        Value::Array(stored_argv.iter().map(|a| json!(a)).collect::<Vec<_>>())
                            .to_string(),
                        json!(env_keys).to_string(),
                        launch_kind,
                        generation,
                        project_id,
                        now() as i64
                    ],
                )
                .map_err(|e| e.to_string())?;
        }
        self.log_event(
            &target,
            "created",
            json!({"profile":profile_id,"cwd":path_text,"kind":launch_kind,"gen":generation}),
        );
        self.log_event(
            &target,
            "launch_argv",
            json!({"argv":stored_argv,"cwd":path_text,"harness":harness,
                "env_keys":env_keys}),
        );
        let mut warnings: Vec<String> = Vec::new();
        if native_id.is_none() && matches!(resume, adapters::Resume::Id(_)) {
            warnings.push("native_id_unresolved".into());
        }
        let command = self.launch_command(&argv, &env);
        // remain-on-exit is global: setting it per session races a pane that
        // dies inside new-session and loses the exit status (BUG-073/080).
        let _ = self.tmux_ok(&["set-option", "-g", "remain-on-exit", "on"]);
        let output = self.tmux_out(&[
            "new-session",
            "-d",
            "-s",
            &target,
            "-x",
            &cols.to_string(),
            "-y",
            &rows_size.to_string(),
            "-c",
            &path_text,
            &command,
        ]);
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                self.mark_session(&target, "exited", None);
                return Err(format!("Could not start the terminal server: {error}"));
            }
        };
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).into_owned();
            self.mark_session(&target, "exited", None);
            return Err(error);
        }
        let _ = self.tmux_ok(&["set-option", "-t", &target, "status", "off"]);
        let _ = self.tmux_ok(&["set-option", "-t", &target, "mouse", "on"]);
        let log_path = self.session_log(&target);
        // Pre-create with 0600: `cat >>` would otherwise inherit the umask.
        let _ = self.open_log(&target);
        let pipe = format!("cat >> {}", adapters::sh_quote(&log_path.to_string_lossy()));
        let _ = self.tmux_ok(&["pipe-pane", "-o", "-t", &target, &pipe]);
        // Settle: watch the pane up to 1.2 s and report what really happened.
        let deadline = Instant::now() + LAUNCH_SETTLE;
        let mut running = false;
        let mut seen = false;
        let mut exit_status = None;
        while Instant::now() < deadline {
            let panes = self.panes();
            match panes.iter().find(|pane| pane.session == target) {
                Some(pane) if pane.dead => {
                    exit_status = pane.dead_status;
                    break;
                }
                Some(pane) => {
                    seen = true;
                    running = true;
                    let state = self.state.lock().unwrap();
                    let _ = state.db.execute(
                        "UPDATE sessions SET pid=?1 WHERE id=?2",
                        params![pane.pid.map(|p| p as i64), target],
                    );
                }
                // Never seen: session propagation lag, keep polling.
                // Seen then gone: the pane vanished without a corpse.
                None if seen => break,
                None => {}
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if running {
            // Confirm once more — a pane can die inside the settle window.
            let panes = self.panes();
            match panes.iter().find(|pane| pane.session == target) {
                Some(pane) if !pane.dead => {}
                Some(pane) => {
                    running = false;
                    exit_status = pane.dead_status;
                }
                None => running = false,
            }
        }
        if !running {
            let screen = self.capture_pane(&target, 400);
            let _ = self.append_screen(&target);
            self.mark_session(&target, "exited", exit_status);
            self.log_event(&target, "exited", json!({"exit_status":exit_status}));
            self.event_for(
                project_id.as_deref(),
                Some(&target),
                "session_state",
                "Harness exited immediately",
                json!({"id":target,"state":"exited","exit_status":exit_status}),
            );
            let payload = json!({
                "id": target,
                "name": name,
                "profile": profile_id,
                "cwd": path_text,
                "state": "exited",
                "running": false,
                "ok": false,
                "exit_status": exit_status,
                "screen": adapters::strip_ansi(&screen),
                "warnings": warnings,
                "error": format!("Harness exited immediately after launch (status {:?})", exit_status),
                "project_id": project_id,
            });
            return Ok(payload);
        }
        self.mark_session(&target, "running", None);
        self.log_event(&target, "ready", json!({"state":"running"}));
        // Native-id capture: preseed returns instantly; everything else polls
        // the registry then the store diff with the shared 10 s budget.
        if native_id.is_none() {
            let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
            let pid = self
                .panes()
                .into_iter()
                .find(|pane| pane.session == target)
                .and_then(|pane| pane.pid);
            if let Some(captured) =
                adapters::capture_native_id(&home, adapter, &path_text, pid, None, &snapshot)
            {
                let state = self.state.lock().unwrap();
                let _ = state.db.execute(
                    "UPDATE sessions SET native_id=?1 WHERE id=?2",
                    params![captured, target],
                );
                drop(state);
                self.log_event(&target, "native_captured", json!({"native_id":captured}));
            } else if matches!(adapter, "claude" | "codex" | "agy") {
                warnings.push("native_id_not_captured".into());
                self.log_event(&target, "native_captured", json!({"native_id":null,"pending":true}));
            }
        } else {
            self.log_event(&target, "native_captured", json!({"native_id":native_id}));
        }
        // Version probing belongs to `harness_list` (60 s cache), never to the
        // launch path: one slow `--version` must not delay a session (BUG-072).
        // The column is filled by discovery/startup, not by create.
        // last_launched is the only implicit write; the voice target is
        // reserved for session_target (BUG-081).
        let mut settings = self.settings();
        settings["last_launched"] = json!(target);
        let _ = self.save_settings(settings, false);
        let mut result = json!({
            "id": target,
            "name": name,
            "profile": profile_id,
            "cwd": path_text,
            "state": "starting",
            "observed_state": "running",
            "running": true,
            "project_id": project_id,
            "harness": harness,
            "native_id": native_id,
            "launch_kind": launch_kind,
            "gen": generation,
            "warnings": warnings,
        });
        if let Some(text) = prompt.filter(|text| !text.trim().is_empty()) {
            match self.inject(&target, text, true) {
                Ok(report) => {
                    result["prompt"] = json!(report);
                }
                Err(error) => {
                    result["prompt"] = json!({"ok":false,"error":error});
                    if let Some(warnings) = result["warnings"].as_array_mut() {
                        warnings.push(json!("prompt_failed"));
                    }
                }
            }
        }
        self.event_for(
            project_id.as_deref(),
            Some(&target),
            "launched",
            "Harness session launched",
            result.clone(),
        );
        Ok(result)
    }

    /// The launch profile an assessment opens when nothing else can judge: the
    /// first profile, in settings order, whose harness reports through the
    /// progress link and whose executable resolves on the launch PATH.
    pub(crate) fn assessment_profile(&self) -> Option<Value> {
        let dirs = self.launch_dirs();
        self.settings()["profiles"].as_array()?.iter().find(|profile| {
            let Some(executable) = profile["executable"].as_str() else {
                return false;
            };
            let harness = profile["harness"].as_str().unwrap_or_else(|| {
                Path::new(executable).file_name().and_then(|name| name.to_str()).unwrap_or("")
            });
            matches!(adapters::adapter_for(harness), "claude" | "codex" | "opencode" | "pi")
                && adapters::resolve(executable, &dirs).is_ok()
        }).cloned()
    }

    pub fn create_session(&self, p: &Value) -> Result<Value> {
        let profile = self.profile(p["profile"].as_str().unwrap_or_default())?;
        let cwd = p["cwd"].as_str().ok_or("Select a project directory")?;
        let name = p["name"]
            .as_str()
            .unwrap_or(profile["name"].as_str().unwrap_or("Session"));
        if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
            return Err("Invalid session name".into());
        }
        // The row must not exist before a resolvable executable does; keep the
        // pre-insert resolution here so failure leaves zero side effects.
        let dirs = self.launch_dirs();
        let executable = profile["executable"].as_str().ok_or("Missing executable")?;
        adapters::resolve(executable, &dirs)?;
        let canonical = std::fs::canonicalize(cwd)
            .map_err(|e| format!("Invalid project directory: {e}"))?;
        if !canonical.is_dir() {
            return Err("Project path must be a directory".into());
        }
        let project_id = {
            let state = self.state.lock().unwrap();
            store::project_id_for_path(&state.db, &canonical.to_string_lossy())
        };
        if project_id.is_none() && p["allow_unregistered"].as_bool() != Some(true) {
            return Err("Register this directory as a project before launching".into());
        }
        let resume = match p["resume"].as_str() {
            None | Some("") => match profile["resume_default"].as_str() {
                Some("last") => adapters::Resume::Last,
                _ => adapters::Resume::New,
            },
            Some("new") => adapters::Resume::New,
            Some("last") => adapters::Resume::Last,
            Some(other) => adapters::Resume::Id(
                other.strip_prefix("id:").unwrap_or(other).to_string(),
            ),
        };
        self.launch_managed(
            &profile,
            &canonical.to_string_lossy(),
            name,
            resume,
            None,
            Some(project_id),
            p["prompt"].as_str(),
            0,
        )
    }

    // ------------------------------------------------------------------
    // Inject: one implementation for prompt/create/resume (BUG-076)
    // ------------------------------------------------------------------

    /// Bracketed-paste inject: the text lands verbatim through tmux's buffer,
    /// then Enter after 120 ms. Readiness waits for a prompt glyph (or a
    /// stable screen) for up to 8 s — it never blind-sleeps.
    pub fn inject(&self, target: &str, text: &str, wait: bool) -> Result<Value> {
        self.require_session(target)?;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() || normalized.len() > INJECT_LIMIT {
            return Err(format!("Prompt must contain 1 to {INJECT_LIMIT} bytes"));
        }
        if normalized
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err("Prompt contains disallowed control characters".into());
        }
        let harness = self
            .rows(false)
            .ok()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row.id == target)
                    .map(|row| row.harness.clone())
            })
            .unwrap_or_default();
        let start = Instant::now();
        let ready = if wait {
            self.wait_ready(target, &harness)
        } else {
            false
        };
        let mut tmux = self.tmux();
        tmux.args(["load-buffer", "-b", "navi-prompt", "-"]);
        tmux.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        let mut child = tmux.spawn().map_err(|e| e.to_string())?;
        let outcome = (|| -> Result<()> {
            child
                .stdin
                .as_mut()
                .ok_or("Buffer pipe missing")?
                .write_all(normalized.as_bytes())
                .map_err(|e| e.to_string())?;
            let output = child.wait_with_output().map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).into_owned());
            }
            let paste = self.tmux_out(&[
                "paste-buffer",
                "-p",
                "-d",
                "-b",
                "navi-prompt",
                "-t",
                target,
            ])?;
            if !paste.status.success() {
                return Err(String::from_utf8_lossy(&paste.stderr).into_owned());
            }
            std::thread::sleep(INJECT_ENTER_DELAY);
            let enter = self.tmux_out(&["send-keys", "-t", target, "Enter"])?;
            if !enter.status.success() {
                return Err(String::from_utf8_lossy(&enter.stderr).into_owned());
            }
            Ok(())
        })();
        let report = json!({
            "ok": outcome.is_ok(),
            "ready": ready,
            "waited_ms": start.elapsed().as_millis() as u64,
            "bytes": normalized.len(),
        });
        let stored: String = normalized.chars().take(2048).collect();
        self.log_event(
            target,
            "prompt",
            json!({
                "text": stored,
                "bytes": normalized.len(),
                "ready": ready,
                "waited_ms": report["waited_ms"],
                "ok": outcome.is_ok(),
                "error": outcome.as_ref().err().cloned(),
            }),
        );
        if outcome.is_ok() {
            // A prompt opens the sampling window: the harness's own output is
            // the only place a delegated test result appears (BUG-087).
            self.request_tail_samples(target);
        }
        outcome.map(|_| report)
    }

    /// Readiness: adapter prompt glyphs, else "screen non-empty and stable for
    /// ~300 ms". Polls at 100 ms for up to 8 s; a dead pane is never ready.
    fn wait_ready(&self, target: &str, harness: &str) -> bool {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last_screen = String::new();
        let mut last_change = Instant::now();
        while Instant::now() < deadline {
            let pane = self
                .panes()
                .into_iter()
                .find(|pane| pane.session == target);
            match pane {
                Some(pane) if !pane.dead => {}
                _ => return false,
            }
            let screen = self.capture_pane(target, 20);
            let trimmed = screen.trim_end().to_string();
            for glyph in adapters::prompt_glyphs(harness) {
                if trimmed
                    .lines()
                    .last()
                    .is_some_and(|line| line.trim_end().ends_with(glyph))
                    && !trimmed.is_empty()
                {
                    return true;
                }
            }
            if trimmed != last_screen {
                last_screen = trimmed;
                last_change = Instant::now();
            } else if !last_screen.is_empty() && last_change.elapsed() >= Duration::from_millis(300)
            {
                return true;
            }
            std::thread::sleep(READY_POLL);
        }
        false
    }

    // ------------------------------------------------------------------
    // Lifecycle RPCs
    // ------------------------------------------------------------------

    /// `session_kill`: deliberate termination; the screen and exit marker are
    /// captured before the session is removed from tmux.
    pub fn session_kill(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?;
        let row = self
            .rows(false)?
            .into_iter()
            .find(|row| row.id == target)
            .ok_or("Unknown session")?;
        let alive = self
            .panes()
            .into_iter()
            .any(|pane| pane.session == target);
        if alive {
            let _ = self.append_screen(target);
            let killed = self.tmux_ok(&["kill-session", "-t", target]);
            if !killed {
                return Err("Could not kill the tmux session".into());
            }
        }
        {
            let state = self.state.lock().unwrap();
            let _ = state.db.execute(
                "UPDATE sessions SET exited=?1 WHERE id=?2 AND exited IS NULL",
                params![now() as i64, target],
            );
        }
        self.mark_session(target, "exited", None);
        self.log_event(target, "exited", json!({"killed":true}));
        self.event_for(
            row.project_id.as_deref(),
            Some(target),
            "session_state",
            "Harness session killed",
            json!({"id":target,"state":"exited"}),
        );
        Ok(json!({"id":target,"killed":alive,"state":"exited"}))
    }

    /// `session_remove`: soft delete — the row and log survive; the chip goes.
    pub fn session_remove(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?;
        let row = self
            .rows(false)?
            .into_iter()
            .find(|row| row.id == target)
            .ok_or("Unknown session")?;
        if self
            .panes()
            .into_iter()
            .any(|pane| pane.session == target)
        {
            let _ = self.append_screen(target);
            let _ = self.tmux_ok(&["kill-session", "-t", target]);
        }
        {
            let state = self.state.lock().unwrap();
            state
                .db
                .execute(
                    "UPDATE sessions SET closed_at=?1,state='removed' WHERE id=?2",
                    params![now() as i64, target],
                )
                .map_err(|e| e.to_string())?;
        }
        self.log_event(target, "removed", json!({}));
        self.event_for(
            row.project_id.as_deref(),
            Some(target),
            "session_state",
            "Harness session removed",
            json!({"id":target,"state":"removed"}),
        );
        Ok(json!({"id":target,"removed":true}))
    }

    /// `session_restart`: a fresh row (`gen+1`, `replaced_by`) that resumes the
    /// native conversation when the harness recorded one.
    pub fn session_restart(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?;
        let row = self
            .rows(false)?
            .into_iter()
            .find(|row| row.id == target)
            .ok_or("Unknown session")?;
        let profile = self.profile(&row.profile)?;
        if self
            .panes()
            .into_iter()
            .any(|pane| pane.session == target)
        {
            let _ = self.append_screen(target);
            let _ = self.tmux_ok(&["kill-session", "-t", target]);
        }
        {
            let state = self.state.lock().unwrap();
            let _ = state.db.execute(
                "UPDATE sessions SET exited=?1 WHERE id=?2 AND exited IS NULL",
                params![now() as i64, target],
            );
        }
        self.mark_session(target, "exited", row.exit_status);
        let resume = match row.native_id.as_deref() {
            Some(native) => adapters::Resume::Id(native.to_string()),
            None => adapters::Resume::New,
        };
        let result = self.launch_managed(
            &profile,
            &row.cwd,
            &row.name,
            resume,
            row.native_id.clone(),
            Some(row.project_id.clone()),
            None,
            row.generation + 1,
        )?;
        if let Some(new_id) = result["id"].as_str() {
            let state = self.state.lock().unwrap();
            let _ = state.db.execute(
                "UPDATE sessions SET replaced_by=?1 WHERE id=?2",
                params![new_id, target],
            );
        }
        Ok(result)
    }

    /// `session_resume`: open a *new* managed pane that continues an existing
    /// native conversation — ours or an external one (BUG-069/074).
    pub fn session_resume(&self, p: &Value) -> Result<Value> {
        // Form A: resume a navi row by id or name.
        let row = if let Some(id) = p["id"].as_str().filter(|id| !id.is_empty()) {
            Some(
                self.rows(true)?
                    .into_iter()
                    .find(|row| row.id == id)
                    .ok_or("Unknown session")?,
            )
        } else if let Some(name) = p["name"].as_str().filter(|name| !name.is_empty()) {
            let rows = self.rows(false)?;
            let matches: Vec<Row> = rows
                .into_iter()
                .filter(|row| row.name == name)
                .collect();
            if matches.len() != 1 {
                return Err(format!("Name matches {} sessions", matches.len()));
            }
            Some(matches.into_iter().next().unwrap())
        } else {
            None
        };
        // Form B: resume by harness + native id (external sessions).
        let (profile_id, cwd, name, native, kind) = if let Some(row) = row {
            let native = row.native_id.clone();
            let kind = if native.is_some() {
                adapters::Resume::Id(native.clone().unwrap_or_default())
            } else {
                adapters::Resume::Last
            };
            (
                row.profile.clone(),
                p["cwd"].as_str().map(str::to_string).unwrap_or(row.cwd.clone()),
                p["name"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{} (resumed)", row.name)),
                native,
                kind,
            )
        } else {
            let harness = p["harness"].as_str().ok_or("Missing harness")?;
            let native = p["native_id"]
                .as_str()
                .map(str::to_string)
                .filter(|id| !id.is_empty());
            if native.is_none() && p["name"].is_null() {
                return Err("Resume needs an id, a name, or a harness + native_id".into());
            }
            let profile_id = self
                .settings()["profiles"]
                .as_array()
                .and_then(|profiles| {
                    profiles
                        .iter()
                        .find(|profile| {
                            adapters::adapter_for(
                                profile["harness"].as_str().unwrap_or(
                                    profile["executable"].as_str().unwrap_or_default(),
                                ),
                            ) == adapters::adapter_for(harness)
                        })
                        .and_then(|profile| profile["id"].as_str().map(str::to_string))
                })
                .ok_or("No profile matches that harness")?;
            let cwd = p["cwd"]
                .as_str()
                .map(str::to_string)
                .or_else(|| self.external_cwd(harness, native.as_deref()))
                .ok_or("Select a project directory")?;
            (
                profile_id,
                cwd,
                format!("{harness} resume"),
                native.clone(),
                match &native {
                    Some(id) => adapters::Resume::Id(id.clone()),
                    None => adapters::Resume::Last,
                },
            )
        };
        let profile = self.profile(&profile_id)?;
        let prompt = p["prompt"].as_str();
        self.launch_managed(&profile, &cwd, &name, kind, native, None, prompt, 0)
    }


    fn external_cwd(&self, harness: &str, native: Option<&str>) -> Option<String> {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        adapters::external_sessions(&home, adapters::adapter_for(harness))
            .into_iter()
            .find(|row| row["native_id"].as_str() == native)
            .and_then(|row| row["cwd"].as_str().map(str::to_string))
            .filter(|cwd| !cwd.is_empty())
    }

    /// `session_target`: the only writer of `active_session` — the explicit
    /// voice target (BUG-081).
    pub fn session_target(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?;
        self.require_session(target)?;
        self.set_active(target)?;
        Ok(json!({"id":target,"active_session":target}))
    }

    /// `session_interrupt`: adapter interrupt keys; `hard` escalates to
    /// kill-session if the pane stays busy for 3 s (BUG-082).
    pub fn session_interrupt(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?;
        let row = self
            .rows(false)?
            .into_iter()
            .find(|row| row.id == target)
            .ok_or("Unknown session")?;
        if !self
            .panes()
            .into_iter()
            .any(|pane| pane.session == target && !pane.dead)
        {
            return Err("Session is not running".into());
        }
        let profile = self.profile(&row.profile).unwrap_or(json!({}));
        let override_keys: Option<Vec<String>> = profile["interrupt_keys"]
            .as_array()
            .filter(|keys| !keys.is_empty())
            .map(|keys| {
                keys.iter()
                    .filter_map(|key| key.as_str().map(str::to_string))
                    .collect()
            });
        let (soft, hard) = adapters::interrupt_keys(&row.harness);
        let keys: Vec<String> = override_keys.unwrap_or_else(|| {
            if p["hard"].as_bool() == Some(true) {
                hard.iter().map(|k| k.to_string()).collect()
            } else {
                soft.iter().map(|k| k.to_string()).collect()
            }
        });
        for key in &keys {
            let sent = self.tmux_ok(&["send-keys", "-t", target, key]);
            if !sent {
                return Err(format!("Could not send {key} to the pane"));
            }
        }
        let mut killed = false;
        if p["hard"].as_bool() == Some(true) {
            let deadline = Instant::now() + HARD_KILL_AFTER;
            loop {
                let pane = self
                    .panes()
                    .into_iter()
                    .find(|pane| pane.session == target);
                match pane {
                    None => break,
                    Some(pane) if pane.dead => break,
                    Some(pane) => {
                        let launcher = self.launcher_command(&row);
                        if pane.command == launcher || SHELL_COMMANDS.contains(&pane.command.as_str())
                        {
                            break;
                        }
                    }
                }
                if Instant::now() >= deadline {
                    let _ = self.append_screen(target);
                    killed = self.tmux_ok(&["kill-session", "-t", target]);
                    if killed {
                        self.mark_session(target, "exited", None);
                        self.log_event(target, "exited", json!({"killed":true,"hard":true}));
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
        self.log_event(
            target,
            "state",
            json!({"state":"interrupted","hard":p["hard"].as_bool()==Some(true),"killed":killed}),
        );
        self.event_for(
            row.project_id.as_deref(),
            Some(target),
            "stopped",
            "Harness turn interrupted",
            json!({"scope":"harness","id":target,"hard":p["hard"].as_bool()==Some(true),"killed":killed}),
        );
        Ok(json!({"id":target,"interrupted":true,"killed":killed}))
    }

    // ------------------------------------------------------------------
    // Output + discovery
    // ------------------------------------------------------------------

    /// `session_output`: ANSI-stripped tail of the pipe log; `since` is a byte
    /// offset so callers can tail without re-reading.
    pub fn session_output(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?;
        let limit = p["limit"].as_u64().unwrap_or(64 * 1024).min(256 * 1024);
        self.maybe_rotate(target);
        let log = self.session_log(target);
        let meta = fs::metadata(&log).map_err(|_| "Session log is empty".to_string())?;
        let size = meta.len();
        let start = p["since"].as_u64().unwrap_or(0).min(size);
        let mut file = fs::File::open(&log).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        let mut buffer = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut buffer)
            .map_err(|e| e.to_string())?;
        let truncated = buffer.len() as u64 > limit;
        buffer.truncate(limit as usize);
        let text = adapters::strip_ansi(&String::from_utf8_lossy(&buffer));
        Ok(json!({
            "id": target,
            "offset": start,
            "next_offset": start + buffer.len() as u64,
            "size": size,
            "truncated": truncated,
            "text": text,
        }))
    }

    // ------------------------------------------------------------------
    // Harness tail -> graph evidence (BUG-087)
    // ------------------------------------------------------------------

    /// `harness_tail`: capture the pane, store one row per distinct screen,
    /// parse it with the profile's versioned rules, and write an
    /// evidence-backed `test_pass`/`test_fail`. An unchanged screen produces
    /// nothing (the sampler reads the same pane for 90 s), and output without a
    /// decisive line produces nothing either.
    pub fn harness_tail(&self, p: &Value) -> Result<Value> {
        let target = p["id"].as_str().ok_or("Missing session")?.to_string();
        let lines = p["lines"]
            .as_i64()
            .unwrap_or(TAIL_LINES_DEFAULT)
            .clamp(1, TAIL_LINES_MAX);
        let row = self
            .rows(false)?
            .into_iter()
            .find(|row| row.id == target)
            .ok_or_else(|| format!("Unknown session: {target}"))?;
        if !self.panes().iter().any(|pane| pane.session == target) {
            return Err("Session pane is gone".into());
        }
        let text = adapters::strip_ansi(&self.capture_pane(&target, lines));
        let recorded = self.record_tail(&target, row.project_id.as_deref(), &text)?;
        let mut out = json!({
            "id": target,
            "lines": lines,
            "hash": recorded["hash"],
            "tail_id": recorded["id"],
            "changed": recorded["changed"],
            "text": clip_tail(&text, 64 * 1024),
            "signals": [],
            "task": Value::Null,
            "blocked": false,
            "rules": {"family": tail_family(&row.harness), "version": TAIL_RULES_VERSION},
        });
        if recorded["changed"] != json!(true) {
            out["duplicate"] = json!(true);
            return Ok(out);
        }
        let Some(project_id) = row.project_id.clone() else {
            // Only a registered project owns a ledger; the tail is still stored.
            out["unscoped"] = json!(true);
            return Ok(out);
        };
        let task = self.tail_task(&project_id, p, &text)?;
        let Some(verdict) = parse_tail(&row.harness, &text) else {
            return Ok(out);
        };
        let kind = if verdict.pass { "test_pass" } else { "test_fail" };
        let value = if verdict.pass { 1.0 } else { 0.0 };
        let signal = self.add_signal(
            &project_id,
            task.as_ref().map(|task| task.id.as_str()),
            task.as_ref().and_then(|task| task.aspect.as_deref()),
            kind,
            "harness",
            Some(value),
            0.9,
            &verdict.line,
        )?;
        let mut blocked = false;
        if !verdict.pass
            && let Some(task) = task.as_ref()
            && !matches!(task.state.as_str(), "blocked" | "dropped")
        {
            let state = self.state.lock().unwrap();
            state
                .db
                .execute(
                    "UPDATE tasks SET state='blocked',updated=?1 WHERE id=?2",
                    params![now(), &task.id],
                )
                .map_err(|error| error.to_string())?;
            drop(state);
            blocked = true;
        }
        let evidence = verdict.line.clone();
        out["signals"] = json!([{
            "id": signal,
            "kind": kind,
            "source": "harness",
            "value": value,
            "confidence": 0.9,
            "evidence": evidence,
            "task_id": task.as_ref().map(|task| task.id.clone()),
            "count": verdict.count,
        }]);
        out["task"] = match task.as_ref() {
            Some(task) => json!({
                "id": task.id,
                "state": if blocked { "blocked".to_string() } else { task.state.clone() },
            }),
            None => Value::Null,
        };
        out["blocked"] = json!(blocked);
        out["rules"] = json!({"family": verdict.family, "version": verdict.rules});
        self.event_for(
            Some(&project_id),
            Some(target.as_str()),
            "harness",
            if blocked {
                "Harness reported a failure: task blocked"
            } else if verdict.pass {
                "Harness reported passing tests"
            } else {
                "Harness reported failing tests"
            },
            json!({
                "session": target,
                "task": task.as_ref().map(|task| task.id.clone()),
                "signal": signal,
                "count": verdict.count,
                "blocked": blocked,
                "rules": verdict.rules,
            }),
        );
        Ok(out)
    }

    /// Which task a tail belongs to: the explicit `task_id`, else the newest
    /// `navi:task=` trailer that names a task in this project.
    fn tail_task(&self, project_id: &str, p: &Value, text: &str) -> Result<Option<TailTask>> {
        let explicit = p["task_id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(id) = explicit {
            return match self.tail_task_row(project_id, id)? {
                Some(task) => Ok(Some(task)),
                None => Err(format!("Unknown task: {id}")),
            };
        }
        for hint in task_hints(text) {
            if let Some(task) = self.tail_task_row(project_id, &hint)? {
                return Ok(Some(task));
            }
        }
        Ok(None)
    }

    fn tail_task_row(&self, project_id: &str, id: &str) -> Result<Option<TailTask>> {
        let state = self.state.lock().unwrap();
        state
            .db
            .query_row(
                "SELECT id,aspect_id,state FROM tasks WHERE id=?1 AND project_id=?2",
                params![id, project_id],
                |row| {
                    Ok(TailTask {
                        id: row.get(0)?,
                        aspect: row.get(1)?,
                        state: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    /// Start the post-prompt schedule for a session. A new prompt replaces the
    /// previous plan, so the sampler only ever follows the session the user is
    /// actually driving.
    pub(crate) fn request_tail_samples(&self, session: &str) {
        let (first, interval, window) = tail_schedule();
        let start = now();
        *self.tail_plan.lock().unwrap() = Some(TailPlan {
            session: session.to_string(),
            next_at: start + first.as_millis() as u64,
            until: start + window.as_millis() as u64,
            interval,
        });
    }

    /// The daemon's one sampler: it sleeps until the plan is due rather than
    /// polling tmux, and does nothing while no prompt is pending.
    pub fn tail_sampler_loop(&self) {
        loop {
            let wait = {
                let plan = self.tail_plan.lock().unwrap();
                match plan.as_ref() {
                    Some(plan) => plan.next_at.saturating_sub(now()).min(200),
                    None => 200,
                }
            };
            if wait > 0 {
                std::thread::sleep(Duration::from_millis(wait));
                continue;
            }
            self.tail_sample_due();
        }
    }

    fn tail_sample_due(&self) {
        let plan = {
            let mut guard = self.tail_plan.lock().unwrap();
            let Some(plan) = guard.clone() else {
                return;
            };
            let current = now();
            if current > plan.until {
                *guard = None;
                return;
            }
            if let Some(active) = guard.as_mut() {
                active.next_at = current + active.interval.as_millis() as u64;
            }
            plan
        };
        if let Err(error) =
            self.harness_tail(&json!({"id":plan.session,"lines":TAIL_LINES_DEFAULT}))
        {
            // A session that disappeared ends its own schedule instead of
            // retrying a pane that no longer exists for the rest of the window.
            self.event(
                "harness",
                "Tail sampling stopped",
                json!({"session":plan.session,"error":error}),
            );
            *self.tail_plan.lock().unwrap() = None;
        }
    }

    /// `harnesses` / `harness_list`: discovered executables, versions, launch-
    /// PATH reachability, capabilities, and whether a profile exists; 60 s
    /// cache, plus one `harness` event per boot for unprofiled finds.
    pub fn harness_list(&self) -> Result<Value> {
        {
            let cache = self.harness_cache.lock().unwrap();
            if let Some((stamp, value)) = cache.as_ref()
                && now().saturating_sub(*stamp) < DISCOVERY_TTL
            {
                return Ok(value.clone());
            }
        }
        let dirs = self.launch_dirs();
        let shell_hits = adapters::shell_lookup(&adapters::KNOWN_HARNESSES);
        // The four names are independent: probe them together so one harness
        // that hangs on `--version` costs its own 800 ms instead of the sum of
        // every slow name (BUG-071 discovery latency).
        let discoveries: Vec<adapters::Discovery> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..adapters::KNOWN_HARNESSES.len())
                .map(|index| {
                    let name = adapters::KNOWN_HARNESSES[index];
                    let dirs = &dirs;
                    let hit = shell_hits.get(name).map(String::as_str);
                    scope.spawn(move || adapters::discover(name, dirs, hit))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        let settings = self.settings();
        let profiles = settings["profiles"].as_array().cloned().unwrap_or_default();
        let mut seen_unprofiled = self.harness_seen.lock().unwrap().clone();
        let mut out = Vec::new();
        for (name, discovery) in adapters::KNOWN_HARNESSES.iter().zip(discoveries) {
            let name = *name;
            let profile_exists = profiles.iter().any(|profile| {
                let harness = profile["harness"].as_str().unwrap_or_default();
                adapters::adapter_for(harness) == name
                    || profile["executable"]
                        .as_str()
                        .and_then(|exe| Path::new(exe).file_name())
                        .and_then(|n| n.to_str())
                        == Some(name)
                    || discovery
                        .path
                        .as_deref()
                        .is_some_and(|path| profile["executable"].as_str() == Some(path))
            });
            if discovery.found && !profile_exists && seen_unprofiled.insert(name.to_string()) {
                self.event(
                    "harness",
                    &format!("Discovered {name} without a launch profile"),
                    json!({"harness":name,"path":discovery.path,"version":discovery.version}),
                );
            }
            out.push(json!({
                "name": name,
                "found": discovery.found,
                "path": discovery.path,
                "version": discovery.version,
                "in_navi_path": discovery.in_navi_path,
                "capabilities": adapters::caps(name).json(),
                "profile_exists": profile_exists,
            }));
        }
        let result = json!({"harnesses": out});
        *self.harness_cache.lock().unwrap() = Some((now(), result.clone()));
        *self.harness_seen.lock().unwrap() = seen_unprofiled;
        Ok(result)
    }

    /// `session_discover`: navi rows merged with external harness sessions,
    /// deduplicated by (harness, native_id) with navi rows winning; the
    /// external readers are 5 s-cached and bounded per harness.
    pub fn session_discover(&self) -> Result<Value> {
        let navi = self.sessions()?;
        let external = {
            let mut cache = self.external_cache.lock().unwrap();
            if let Some((stamp, rows)) = cache.as_ref()
                && now().saturating_sub(*stamp) < EXTERNAL_TTL
            {
                rows.clone()
            } else {
                let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
                let mut rows = Vec::new();
                for adapter in ["claude", "codex", "agy"] {
                    rows.extend(adapters::external_sessions(&home, adapter));
                }
                *cache = Some((now(), rows.clone()));
                rows
            }
        };
        let mut owned: BTreeSet<String> = BTreeSet::new();
        let mut out: Vec<Value> = Vec::new();
        for row in &navi {
            if let (Some(harness), Some(native)) = (
                non_empty(row["harness"].as_str()),
                row["native_id"].as_str(),
            ) {
                owned.insert(format!("{}:{}", adapters::adapter_for(harness), native));
            }
            let mut row = row.clone();
            row["origin"] = json!("navi");
            out.push(row);
        }
        for row in external {
            let key = format!(
                "{}:{}",
                row["harness"].as_str().unwrap_or_default(),
                row["native_id"].as_str().unwrap_or_default()
            );
            if owned.contains(&key) {
                continue;
            }
            out.push(row);
        }
        out.sort_by_key(|row| std::cmp::Reverse(row["updated"].as_u64().unwrap_or(0)));
        Ok(json!({"sessions": out}))
    }

    /// Cancel path helper: soft-interrupt busy harnesses (BUG-082).
    pub fn interrupt_busy_harnesses(&self) -> Vec<String> {
        let rows = self.reconcile_rows(false).unwrap_or_default();
        let mut hit = Vec::new();
        for row in rows {
            if !matches!(row.state.as_str(), "busy" | "needs_input") {
                continue;
            }
            let (soft, _) = adapters::interrupt_keys(&row.harness);
            for key in soft {
                let _ = self.tmux_ok(&["send-keys", "-t", &row.id, key]);
            }
            self.event_for(
                row.project_id.as_deref(),
                Some(&row.id),
                "stopped",
                "Harness turn interrupted",
                json!({"scope":"harness","id":row.id}),
            );
            hit.push(row.id.clone());
        }
        hit
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// The parse table is pure, so it is tested without a daemon: a delegated
/// harness's output is the only source that can turn into a test signal, and a
/// wrong read here would be a fabricated completion (BUG-087).
#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(text: &str) -> Option<TailVerdict> {
        parse_tail("shell", text)
    }

    #[test]
    fn reads_a_passed_line() {
        let read = verdict("$ run tests\nPASSED 12").expect("PASSED 12 is a test result");
        assert!(read.pass, "{read:?}");
        assert_eq!(read.count, Some(12));
        assert_eq!(read.line, "PASSED 12");
        assert_eq!(read.family, "shell");
        assert_eq!(read.rules, TAIL_RULES_VERSION);
    }

    #[test]
    fn reads_a_failed_line() {
        let read = verdict("$ run tests\nFAILED 3").expect("FAILED 3 is a test result");
        assert!(!read.pass, "{read:?}");
        assert_eq!(read.count, Some(3));
        assert_eq!(read.line, "FAILED 3");
    }

    #[test]
    fn random_output_is_not_a_signal() {
        assert!(verdict("hello world\nbuilding…\nnothing to report").is_none());
        assert!(verdict("").is_none());
    }

    #[test]
    fn the_newest_decisive_line_wins() {
        let read = verdict("PASSED 12\n...\nFAILED 3").expect("the last report decides");
        assert!(!read.pass, "{read:?}");
        assert_eq!(read.count, Some(3));
    }

    #[test]
    fn a_zero_failure_count_is_not_a_failure() {
        let read = verdict("12 passed, 0 failed").expect("a pass line");
        assert!(read.pass, "{read:?}");
        assert_eq!(read.count, Some(12));
        assert!(verdict("0 failed").is_none());
        assert!(verdict("0 passed").is_none());
    }

    #[test]
    fn reads_common_runner_summaries() {
        // pytest
        let pytest = verdict("3 passed in 0.42s").expect("pytest summary");
        assert!(pytest.pass);
        assert_eq!(pytest.count, Some(3));
        // cargo test
        let cargo = verdict("test result: FAILED. 11 passed; 2 failed; 0 ignored").expect("cargo");
        assert!(!cargo.pass, "a non-zero failure count outranks the pass on the same line");
        // go test
        let go = verdict("ok  example.com/pkg  0.02s\nPASS").expect("go summary");
        assert!(go.pass);
        assert_eq!(go.count, None);
    }

    #[test]
    fn a_task_trailer_is_found_once_per_line() {
        assert_eq!(task_hints("done\nnavi:task=tsk-1234\n"), vec!["tsk-1234"]);
        assert_eq!(
            task_hints("navi:task=tsk-a, navi:task=tsk-b)"),
            vec!["tsk-a", "tsk-b"]
        );
        assert!(task_hints("no trailer here").is_empty());
        assert!(task_hints("navi:task=").is_empty());
    }

    #[test]
    fn the_sample_schedule_is_configurable() {
        // SAFETY: this unit test owns the variable and runs before any daemon
        // thread reads it; the value is restored at the end of the test.
        let previous = std::env::var("NAVI_TAIL_SAMPLE_MS").ok();
        unsafe { std::env::set_var("NAVI_TAIL_SAMPLE_MS", "150") };
        let (first, interval, window) = tail_schedule();
        unsafe { std::env::remove_var("NAVI_TAIL_SAMPLE_MS") };
        assert_eq!(first, Duration::from_millis(150));
        assert_eq!(interval, Duration::from_millis(150));
        assert_eq!(window, Duration::from_millis(900));
        if let Some(previous) = previous {
            unsafe { std::env::set_var("NAVI_TAIL_SAMPLE_MS", previous) };
        }
    }
}
