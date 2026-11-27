//! Always-on execution runner for background hands.
//!
//! [`HandsRunner`] drives a [`HandScheduler`]: it polls time-triggered hands in
//! [`HandsRunner::tick`] and dispatches event-triggered hands in
//! [`HandsRunner::emit_event`]. One executor failure never stops the other
//! hands in the same batch. Every outcome is redacted, recorded in a bounded
//! per-hand history, appended to an optional tamper-evident audit log, and
//! persisted atomically as JSON so a restart can continue where it left off.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use navi_sandbox::audit_log::AuditLog;
use navi_sandbox::secret_guard::redact_secrets;
use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

use crate::scheduler::{HandScheduler, HandSpec};

/// Executes one dispatched hand and returns a human-readable output.
///
/// An error is treated as a failed hand run; the runner records it and keeps
/// dispatching the remaining hands in the batch.
pub trait HandExecutor {
    /// Executes `spec` at `now_ms` (milliseconds since an external clock epoch).
    fn execute(&mut self, spec: &HandSpec, now_ms: u64) -> NaviResult<String>;
}

/// Counts the hands dispatched and resolved by one runner call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandTickSummary {
    /// Timestamp passed to the runner call.
    pub ticked_at_ms: u64,
    /// Number of hands moved from `Running` to a terminal result.
    pub started: usize,
    /// Number of hands whose executor returned `Ok`.
    pub succeeded: usize,
    /// Number of hands whose executor returned `Err`.
    pub failed: usize,
}

impl HandTickSummary {
    /// Renders counts for an audit-log outcome field.
    fn outcome(&self) -> String {
        format!(
            "started={} succeeded={} failed={}",
            self.started, self.succeeded, self.failed
        )
    }
}

/// One redacted executor result kept in the runner's output history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandOutputRecord {
    /// Id of the hand that produced the output.
    pub hand_id: String,
    /// Time at which the outcome was recorded.
    pub finished_at_ms: u64,
    /// Whether the executor succeeded.
    pub success: bool,
    /// Executor output on success, or redacted error text on failure.
    pub output: String,
}

/// Current schema version of the persisted runner state.
const STATE_VERSION: u32 = 1;
/// Maximum number of recent outputs retained per hand.
const MAX_OUTPUTS_PER_HAND: usize = 32;
/// Principal written to every audit record emitted by the runner.
const AUDIT_ACTOR: &str = "hands-runner";
/// Maximum number of output characters copied into an audit outcome.
const AUDIT_OUTCOME_LIMIT: usize = 512;

/// State written to the runner's JSON file.
#[derive(Serialize)]
struct StateSnapshot<'a> {
    version: u32,
    scheduler: &'a HandScheduler,
    outputs: &'a BTreeMap<String, Vec<HandOutputRecord>>,
    last_tick: Option<HandTickSummary>,
}

/// Owned form used when reading the runner's JSON file back.
#[derive(Deserialize)]
struct PersistedState {
    #[serde(default = "default_state_version")]
    version: u32,
    #[serde(default)]
    scheduler: HandScheduler,
    #[serde(default)]
    outputs: BTreeMap<String, Vec<HandOutputRecord>>,
    #[serde(default)]
    last_tick: Option<HandTickSummary>,
}

fn default_state_version() -> u32 {
    STATE_VERSION
}

/// Drives background hands, records their outcomes, and persists runner state.
pub struct HandsRunner {
    scheduler: HandScheduler,
    state_path: PathBuf,
    audit_log_path: Option<PathBuf>,
    audit_log: Option<AuditLog>,
    outputs: BTreeMap<String, Vec<HandOutputRecord>>,
    last_tick: Option<HandTickSummary>,
}

impl HandsRunner {
    /// Creates a runner without an audit log, loading any existing state.
    ///
    /// A missing state file yields an empty runner. A present but invalid file
    /// is rejected instead of being silently overwritten.
    pub fn new(state_path: impl AsRef<Path>) -> NaviResult<Self> {
        Self::with_optional_audit_log(state_path, None)
    }

    /// Creates a runner that appends its outcomes to `audit_log_path`.
    pub fn with_audit_log(
        state_path: impl AsRef<Path>,
        audit_log_path: impl AsRef<Path>,
    ) -> NaviResult<Self> {
        Self::with_optional_audit_log(state_path, Some(audit_log_path.as_ref().to_path_buf()))
    }

    /// Creates a runner with an optional audit log, loading any existing state.
    pub fn with_optional_audit_log(
        state_path: impl AsRef<Path>,
        audit_log_path: Option<PathBuf>,
    ) -> NaviResult<Self> {
        let audit_log = audit_log_path.as_ref().map(AuditLog::open).transpose()?;
        let mut runner = Self {
            scheduler: HandScheduler::new(),
            state_path: state_path.as_ref().to_path_buf(),
            audit_log_path,
            audit_log,
            outputs: BTreeMap::new(),
            last_tick: None,
        };
        match runner.reload_state() {
            Ok(()) | Err(NaviError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        Ok(runner)
    }

    /// Returns the scheduler owned by this runner.
    pub fn scheduler(&self) -> &HandScheduler {
        &self.scheduler
    }

    /// Returns mutable access to the scheduler for registration and pause/resume.
    ///
    /// Changes made through this handle are persisted by the next tick or by
    /// an explicit [`Self::save_state`] call.
    pub fn scheduler_mut(&mut self) -> &mut HandScheduler {
        &mut self.scheduler
    }

    /// Registers a hand and persists the updated state.
    pub fn register(&mut self, spec: HandSpec) -> NaviResult<()> {
        self.scheduler.register(spec)?;
        self.save_state()
    }

    /// Registers a hand in memory without writing the state file.
    pub fn register_in_memory(&mut self, spec: HandSpec) -> NaviResult<()> {
        self.scheduler.register(spec)
    }

    /// Path of the runner's JSON state file.
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Path of the audit log, when auditing is enabled.
    pub fn audit_log_path(&self) -> Option<&Path> {
        self.audit_log_path.as_deref()
    }

    /// Summary of the most recent tick or event dispatch.
    pub fn last_tick(&self) -> Option<HandTickSummary> {
        self.last_tick
    }

    /// Redacted outputs recorded for `hand_id`, oldest first.
    pub fn output_history(&self, hand_id: &str) -> &[HandOutputRecord] {
        self.outputs
            .get(hand_id)
            .map_or(&[], |records| records.as_slice())
    }

    /// Most recent redacted output recorded for `hand_id`, if any.
    pub fn last_output(&self, hand_id: &str) -> Option<&str> {
        self.outputs
            .get(hand_id)
            .and_then(|records| records.last())
            .map(|record| record.output.as_str())
    }

    /// Dispatches every time-triggered hand due at `now_ms`.
    pub fn tick(
        &mut self,
        now_ms: u64,
        executor: &mut dyn HandExecutor,
    ) -> NaviResult<HandTickSummary> {
        let due = self.scheduler.due_hands(now_ms);
        let summary = self.dispatch(due, now_ms, executor)?;
        self.audit(now_ms, "hands.tick", "scheduler", &summary.outcome())?;
        Ok(summary)
    }

    /// Dispatches every hand subscribed to `topic` at `now_ms`.
    pub fn emit_event(
        &mut self,
        topic: &str,
        now_ms: u64,
        executor: &mut dyn HandExecutor,
    ) -> NaviResult<HandTickSummary> {
        let triggered = self.scheduler.trigger_event(topic, now_ms);
        let summary = self.dispatch(triggered, now_ms, executor)?;
        let target = redact_secrets(topic);
        self.audit(now_ms, "hands.event", &target, &summary.outcome())?;
        Ok(summary)
    }

    /// Writes runner state to disk using a temporary file and atomic rename.
    pub fn save_state(&self) -> NaviResult<()> {
        let snapshot = StateSnapshot {
            version: STATE_VERSION,
            scheduler: &self.scheduler,
            outputs: &self.outputs,
            last_tick: self.last_tick,
        };
        let encoded = serde_json::to_string_pretty(&snapshot)?;
        atomic_write_json(&self.state_path, &encoded)
    }

    /// Reloads runner state from disk and recovers hands left `Running`.
    pub fn reload_state(&mut self) -> NaviResult<()> {
        let bytes = fs::read(self.state_path.as_path()).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                NaviError::NotFound(format!(
                    "hands state not found: {}",
                    self.state_path.display()
                ))
            } else {
                NaviError::from(err)
            }
        })?;
        let mut state: PersistedState = serde_json::from_slice(&bytes)?;
        if state.version != STATE_VERSION {
            return Err(NaviError::Protocol(format!(
                "unsupported hands state version {}",
                state.version
            )));
        }
        // State files can be edited outside the process, so never trust that
        // previously recorded outputs are still free of credentials.
        for records in state.outputs.values_mut() {
            for record in records {
                record.output = redact_secrets(&record.output);
            }
        }
        state.scheduler.recover_running();
        self.scheduler = state.scheduler;
        self.outputs = state.outputs;
        self.last_tick = state.last_tick;
        Ok(())
    }

    /// Executes a batch of hands that the scheduler already marked `Running`.
    fn dispatch(
        &mut self,
        ids: Vec<String>,
        now_ms: u64,
        executor: &mut dyn HandExecutor,
    ) -> NaviResult<HandTickSummary> {
        let started = ids.len();
        let mut succeeded = 0;
        let mut failed = 0;

        for id in ids {
            let spec = self.scheduler.get(&id).cloned().ok_or_else(|| {
                NaviError::Internal(format!("dispatched hand '{id}' is not registered"))
            })?;
            match executor.execute(&spec, now_ms) {
                Ok(output) => {
                    // Redact before truncating so a secret split across the
                    // audit limit cannot survive as a readable fragment.
                    let redacted = redact_secrets(&output);
                    self.record_output(&id, now_ms, true, &output);
                    self.scheduler.record_success(&id, now_ms)?;
                    succeeded += 1;
                    self.audit(
                        now_ms,
                        "hands.execute",
                        &id,
                        &format!("success: {}", summarize(&redacted)),
                    )?;
                }
                Err(err) => {
                    let message = redact_secrets(&err.to_string());
                    self.record_output(&id, now_ms, false, &message);
                    self.scheduler
                        .record_failure_with_error(&id, message.as_str(), now_ms)?;
                    failed += 1;
                    self.audit(
                        now_ms,
                        "hands.execute",
                        &id,
                        &format!("failure: {}", summarize(&message)),
                    )?;
                }
            }
        }

        let summary = HandTickSummary {
            ticked_at_ms: now_ms,
            started,
            succeeded,
            failed,
        };
        self.last_tick = Some(summary);
        self.save_state()?;
        Ok(summary)
    }

    /// Appends a redacted output record, keeping at most the newest entries.
    fn record_output(&mut self, hand_id: &str, finished_at_ms: u64, success: bool, output: &str) {
        let records = self.outputs.entry(hand_id.to_owned()).or_default();
        records.push(HandOutputRecord {
            hand_id: hand_id.to_owned(),
            finished_at_ms,
            success,
            output: redact_secrets(output),
        });
        if records.len() > MAX_OUTPUTS_PER_HAND {
            records.remove(0);
        }
    }

    /// Appends one record when an audit log is configured.
    fn audit(&mut self, now_ms: u64, action: &str, target: &str, outcome: &str) -> NaviResult<()> {
        if let Some(log) = self.audit_log.as_mut() {
            log.append(now_ms, AUDIT_ACTOR, action, target, outcome)?;
        }
        Ok(())
    }
}

/// Writes `payload` to `path` via a same-directory temporary file and rename.
fn atomic_write_json(path: &Path, payload: &str) -> NaviResult<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let file_name = path.file_name().ok_or_else(|| {
        NaviError::InvalidInput(format!("state path has no file name: {}", path.display()))
    })?;

    for _ in 0..16 {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".tmp.{}.{nonce}", std::process::id()));
        let temp_path = path.with_file_name(temp_name);

        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path);
        let mut file = match opened {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        };

        let written = (|| -> NaviResult<()> {
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp_path, path)?;
            Ok(())
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&temp_path);
            return Err(err);
        }
        // Some platforms cannot fsync a directory handle; durability of the
        // file contents is guaranteed by the sync above regardless.
        if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty())
            && let Ok(dir) = File::open(parent)
        {
            let _ = dir.sync_all();
        }
        return Ok(());
    }

    Err(NaviError::Io(format!(
        "could not create a temporary state file for {}",
        path.display()
    )))
}

/// Truncates text copied into audit outcomes, marking elided content.
fn summarize(text: &str) -> String {
    const LIMIT: usize = AUDIT_OUTCOME_LIMIT;
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let mut prefix = text.chars().take(LIMIT).collect::<String>();
    prefix.push_str("...");
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{HandStatus, HandTrigger};
    use navi_sandbox::audit_log::verify_chain;
    use navi_sandbox::secret_guard::REDACTED;
    use std::sync::atomic::AtomicU64 as TestAtomicU64;

    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: TestAtomicU64 = TestAtomicU64::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "navi-hands-runner-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp dir");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }

        fn file_count(&self) -> usize {
            fs::read_dir(&self.root).expect("read temp dir").count()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct ScriptedExecutor {
        outputs: Vec<String>,
        next: usize,
    }

    impl ScriptedExecutor {
        fn new(outputs: Vec<String>) -> Self {
            Self { outputs, next: 0 }
        }
    }

    impl HandExecutor for ScriptedExecutor {
        fn execute(&mut self, _spec: &HandSpec, _now_ms: u64) -> NaviResult<String> {
            let output = self.outputs.get(self.next).ok_or_else(|| {
                NaviError::Internal("no scripted output left for hand".to_owned())
            })?;
            self.next += 1;
            Ok(output.clone())
        }
    }

    struct SelectiveExecutor {
        fail_id: &'static str,
    }

    impl HandExecutor for SelectiveExecutor {
        fn execute(&mut self, spec: &HandSpec, _now_ms: u64) -> NaviResult<String> {
            if spec.id == self.fail_id {
                Err(NaviError::Internal(format!(
                    "boom ghp_secret123 for {}",
                    spec.id
                )))
            } else {
                Ok(format!("ok {}", spec.id))
            }
        }
    }

    struct ConstantExecutor(&'static str);

    impl HandExecutor for ConstantExecutor {
        fn execute(&mut self, _spec: &HandSpec, _now_ms: u64) -> NaviResult<String> {
            Ok(self.0.to_owned())
        }
    }

    fn spec(id: &str, trigger: HandTrigger) -> HandSpec {
        HandSpec::new(id, format!("hand {id}"), trigger)
    }

    #[test]
    fn tick_executes_due_hands_and_persists_redacted_output() {
        let temp = TempDir::new("tick-redact");
        let state_path = temp.path("state.json");
        let mut runner = HandsRunner::new(state_path.as_path()).expect("create runner");
        runner
            .register(spec("scan", HandTrigger::IntervalMs(1_000)))
            .expect("register scan");

        let mut executor = ScriptedExecutor::new(vec!["done sk-abc123456".to_owned()]);
        let summary = runner.tick(100, &mut executor).expect("tick");

        assert_eq!(
            summary,
            HandTickSummary {
                ticked_at_ms: 100,
                started: 1,
                succeeded: 1,
                failed: 0,
            }
        );
        assert_eq!(runner.last_tick(), Some(summary));
        assert_eq!(
            runner.scheduler().status("scan"),
            Some(HandStatus::Succeeded)
        );
        assert_eq!(runner.scheduler().next_due_ms("scan"), Some(1_100));
        let output = runner.last_output("scan").expect("recorded output");
        assert!(output.contains(REDACTED));
        assert!(!output.contains("sk-abc123456"));

        let reloaded = HandsRunner::new(state_path.as_path()).expect("reload runner");
        assert_eq!(reloaded.last_output("scan"), Some(output));
        assert_eq!(reloaded.scheduler().next_due_ms("scan"), Some(1_100));
        assert_eq!(reloaded.state_path(), state_path.as_path());
        assert_eq!(reloaded.audit_log_path(), None);
    }

    #[test]
    fn tick_isolates_executor_failures_and_redacts_error_records() {
        let temp = TempDir::new("isolate-failure");
        let mut runner = HandsRunner::new(temp.path("state.json")).expect("create runner");
        runner
            .register_in_memory(spec("alpha", HandTrigger::IntervalMs(1_000)))
            .expect("register alpha");
        runner
            .register_in_memory(spec("beta", HandTrigger::IntervalMs(1_000)))
            .expect("register beta");

        let mut executor = SelectiveExecutor { fail_id: "alpha" };
        let summary = runner.tick(10, &mut executor).expect("tick");

        assert_eq!(summary.started, 2);
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(runner.scheduler().status("alpha"), Some(HandStatus::Failed));
        assert_eq!(runner.scheduler().retries("alpha"), Some(1));
        assert_eq!(
            runner.scheduler().status("beta"),
            Some(HandStatus::Succeeded)
        );

        let history = runner.scheduler().history("alpha");
        assert_eq!(history.len(), 1);
        let error = history[0].error.as_deref().expect("failure description");
        assert!(error.contains(REDACTED));
        assert!(!error.contains("ghp_secret123"));
        let output = runner.last_output("alpha").expect("failure output");
        assert_eq!(output, error);
        assert!(
            !runner
                .last_output("beta")
                .expect("beta output")
                .contains(REDACTED)
        );
    }

    #[test]
    fn emit_event_dispatches_only_matching_subscribers() {
        let temp = TempDir::new("event-dispatch");
        let mut runner = HandsRunner::new(temp.path("state.json")).expect("create runner");
        runner
            .register_in_memory(spec("mail", HandTrigger::EventTopic("inbox".to_owned())))
            .expect("register mail");
        runner
            .register_in_memory(spec("calendar", HandTrigger::EventTopic("cal".to_owned())))
            .expect("register calendar");
        runner
            .register_in_memory(spec("poll", HandTrigger::IntervalMs(5_000)))
            .expect("register poll");

        let mut executor = ConstantExecutor("ok");
        let event = runner
            .emit_event("inbox", 1_000, &mut executor)
            .expect("emit");
        assert_eq!(event.started, 1);
        assert_eq!(event.succeeded, 1);
        assert_eq!(
            runner.scheduler().status("mail"),
            Some(HandStatus::Succeeded)
        );
        assert_eq!(
            runner.scheduler().status("calendar"),
            Some(HandStatus::Idle)
        );
        assert_eq!(runner.scheduler().status("poll"), Some(HandStatus::Idle));

        let tick = runner.tick(1_001, &mut executor).expect("tick");
        assert_eq!(tick.started, 1);
        assert_eq!(
            runner.scheduler().status("poll"),
            Some(HandStatus::Succeeded)
        );

        let repeated = runner
            .emit_event("inbox", 1_002, &mut executor)
            .expect("repeat");
        assert_eq!(repeated.started, 1);
        assert_eq!(runner.scheduler().history("mail").len(), 2);
    }

    #[test]
    fn tick_without_due_hands_returns_a_zero_summary() {
        let temp = TempDir::new("no-due");
        let state_path = temp.path("state.json");
        let mut runner = HandsRunner::new(state_path.as_path()).expect("create runner");
        runner
            .register_in_memory(spec("future", HandTrigger::OneShotAtMs(5_000)))
            .expect("register future");
        let mut disabled = spec("paused", HandTrigger::IntervalMs(0));
        disabled.enabled = false;
        runner
            .register_in_memory(disabled)
            .expect("register paused");
        let running = runner.scheduler_mut().due_hands(0);
        assert!(running.is_empty());

        let mut executor = ConstantExecutor("unused");
        let empty = runner.tick(0, &mut executor).expect("empty tick");
        assert_eq!(
            empty,
            HandTickSummary {
                ticked_at_ms: 0,
                started: 0,
                succeeded: 0,
                failed: 0,
            }
        );
        assert!(state_path.exists());
        assert!(runner.output_history("future").is_empty());
        assert_eq!(runner.last_output("missing"), None);
    }

    #[test]
    fn state_roundtrip_recovers_running_hands_and_rejects_corrupt_json() {
        let temp = TempDir::new("state-recovery");
        let state_path = temp.path("nested/state.json");
        {
            let mut runner = HandsRunner::new(state_path.as_path()).expect("create runner");
            runner
                .register(spec("job", HandTrigger::IntervalMs(10)))
                .expect("register job");
            assert_eq!(runner.scheduler_mut().due_hands(0), ["job"]);
            runner.save_state().expect("save state");
        }

        let runner = HandsRunner::new(state_path.as_path()).expect("reload runner");
        assert_eq!(runner.scheduler().status("job"), Some(HandStatus::Idle));
        assert_eq!(runner.scheduler().next_due_ms("job"), Some(0));
        assert_eq!(temp.file_count(), 1, "temporary files must be cleaned up");

        fs::write(state_path.as_path(), "{not json").expect("corrupt state");
        assert!(matches!(
            HandsRunner::new(state_path.as_path()),
            Err(NaviError::Protocol(_))
        ));
    }

    #[test]
    fn audit_log_records_dispatch_without_leaking_secrets() {
        let temp = TempDir::new("audit");
        let state_path = temp.path("state.json");
        let audit_path = temp.path("audit.jsonl");
        let mut runner = HandsRunner::with_audit_log(state_path.as_path(), audit_path.as_path())
            .expect("create runner");
        assert_eq!(runner.audit_log_path(), Some(audit_path.as_path()));
        runner
            .register_in_memory(spec("scan", HandTrigger::OneShotAtMs(2_000)))
            .expect("register scan");

        let mut executor = ScriptedExecutor::new(vec!["key=sk-abcdef123 done".to_owned()]);
        runner.tick(2_000, &mut executor).expect("tick");

        assert_eq!(verify_chain(audit_path.as_path()).expect("verify chain"), 2);
        let contents = fs::read_to_string(audit_path.as_path()).expect("read audit log");
        assert!(contents.contains("hands.execute"));
        assert!(contents.contains("hands.tick"));
        assert!(contents.contains(REDACTED));
        assert!(!contents.contains("sk-abcdef123"));
    }

    #[test]
    fn output_history_is_bounded_and_redacts_reloaded_entries() {
        let temp = TempDir::new("bounded-history");
        let state_path = temp.path("state.json");
        let mut runner = HandsRunner::new(state_path.as_path()).expect("create runner");
        runner
            .register_in_memory(spec("loop", HandTrigger::IntervalMs(0)))
            .expect("register loop");

        let runs = MAX_OUTPUTS_PER_HAND + 5;
        let mut executor = ConstantExecutor("plain output");
        for now_ms in 0..runs as u64 {
            runner.tick(now_ms, &mut executor).expect("tick");
        }

        let history = runner.output_history("loop");
        assert_eq!(history.len(), MAX_OUTPUTS_PER_HAND);
        assert_eq!(history[0].finished_at_ms, 5);
        assert_eq!(
            history[MAX_OUTPUTS_PER_HAND - 1].finished_at_ms,
            runs as u64 - 1
        );
        assert_eq!(runner.last_output("loop"), Some("plain output"));

        let raw = fs::read_to_string(state_path.as_path()).expect("read state");
        let tampered = raw.replace("plain output", "secret ghp_zzz999");
        fs::write(state_path.as_path(), tampered).expect("tamper state");
        let reloaded = HandsRunner::new(state_path.as_path()).expect("reload runner");
        let last = reloaded.last_output("loop").expect("reloaded output");
        assert!(last.contains(REDACTED));
        assert!(!last.contains("ghp_zzz999"));
    }
}
