//! Always-on autonomous background Hands scheduler.
//!
//! A [`HandScheduler`] owns a set of registered background hands and decides
//! when each one should run. Three triggers are supported: periodic intervals,
//! published event topics, and one-shot absolute deadlines. Dispatch is a
//! two-step protocol: [`HandScheduler::due_hands`] (or
//! [`HandScheduler::trigger_event`]) returns the ids of hands that were moved
//! into `Running`, and the caller later reports the outcome with
//! [`HandScheduler::record_success`] or [`HandScheduler::record_failure`].
//! Failures reschedule the hand using exponential backoff until
//! `max_retries` is exhausted. The complete state can be persisted as JSON.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Condition that makes a hand eligible to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandTrigger {
    /// Run every `ms` milliseconds, measured from the last successful run.
    ///
    /// A value of zero means the hand is due on every poll.
    IntervalMs(u64),
    /// Run whenever an event with exactly this topic is published.
    EventTopic(String),
    /// Run once at or after this absolute timestamp (milliseconds).
    OneShotAtMs(u64),
}

/// Runtime lifecycle state of a hand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandStatus {
    /// Eligible to run when its trigger fires.
    #[default]
    Idle,
    /// Dispatched and waiting for a result.
    Running,
    /// Last run succeeded.
    Succeeded,
    /// Last run failed; the hand may still have retries left.
    Failed,
    /// Explicitly paused and never dispatched.
    Paused,
}

/// Static configuration of a background hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandSpec {
    /// Unique identifier used for dispatch and result reporting.
    pub id: String,
    /// Human-readable label.
    pub name: String,
    /// What causes the hand to run.
    pub trigger: HandTrigger,
    /// Number of retries allowed after the initial failed attempt.
    pub max_retries: u32,
    /// Base delay for exponential backoff, in milliseconds.
    pub backoff_ms: u64,
    /// Whether the hand may be dispatched at all.
    pub enabled: bool,
}

impl HandSpec {
    /// Creates a spec with `max_retries = 3`, `backoff_ms = 1000` and enabled.
    pub fn new(id: impl Into<String>, name: impl Into<String>, trigger: HandTrigger) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            trigger,
            max_retries: 3,
            backoff_ms: 1_000,
            enabled: true,
        }
    }
}

/// Outcome of a single dispatched hand run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandRunRecord {
    /// Id of the hand that ran.
    pub hand_id: String,
    /// One-based number of the attempt, including the initial attempt.
    pub attempt: u32,
    /// Time at which the hand was dispatched, in milliseconds.
    pub started_at_ms: u64,
    /// Time at which the outcome was recorded, in milliseconds.
    pub finished_at_ms: u64,
    /// Whether the run succeeded.
    pub success: bool,
    /// Failure description, if the run failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Mutable state kept for one registered hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HandState {
    spec: HandSpec,
    #[serde(default)]
    status: HandStatus,
    #[serde(default)]
    retries: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_due_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_started_ms: Option<u64>,
    #[serde(default)]
    history: Vec<HandRunRecord>,
}

/// In-memory registry and dispatch state for all background hands.
///
/// Hands are kept in id order so dispatch results are deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandScheduler {
    hands: BTreeMap<String, HandState>,
}

impl HandScheduler {
    /// Creates an empty scheduler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a hand and initializes its runtime state.
    ///
    /// Interval hands are immediately due; one-shot hands become due at their
    /// deadline; event hands only run through [`Self::trigger_event`]. A hand
    /// registered with `enabled = false` starts paused. Duplicate and blank
    /// ids are rejected.
    pub fn register(&mut self, spec: HandSpec) -> NaviResult<()> {
        if spec.id.trim().is_empty() {
            return Err(NaviError::InvalidInput(
                "hand id must not be empty".to_owned(),
            ));
        }
        if self.hands.contains_key(&spec.id) {
            return Err(NaviError::InvalidInput(format!(
                "hand '{}' is already registered",
                spec.id
            )));
        }

        let next_due_ms = match &spec.trigger {
            HandTrigger::IntervalMs(_) => Some(0),
            HandTrigger::OneShotAtMs(at) => Some(*at),
            HandTrigger::EventTopic(_) => None,
        };
        let status = if spec.enabled {
            HandStatus::Idle
        } else {
            HandStatus::Paused
        };
        self.hands.insert(
            spec.id.clone(),
            HandState {
                spec,
                status,
                retries: 0,
                next_due_ms,
                last_started_ms: None,
                history: Vec::new(),
            },
        );
        Ok(())
    }

    /// Pauses a hand so it is never dispatched until resumed.
    ///
    /// Pausing an already paused hand is a no-op. Unknown ids are an error.
    pub fn pause(&mut self, id: &str) -> NaviResult<()> {
        let state = self
            .hands
            .get_mut(id)
            .ok_or_else(|| NaviError::NotFound(format!("hand '{id}' not found")))?;
        state.status = HandStatus::Paused;
        Ok(())
    }

    /// Resumes a paused hand and re-enables it for dispatch.
    ///
    /// Resuming a hand that is not paused leaves its other state untouched.
    pub fn resume(&mut self, id: &str) -> NaviResult<()> {
        let state = self
            .hands
            .get_mut(id)
            .ok_or_else(|| NaviError::NotFound(format!("hand '{id}' not found")))?;
        state.status = HandStatus::Idle;
        state.spec.enabled = true;
        Ok(())
    }

    /// Returns the ids of time-triggered hands due at `now_ms` and marks them running.
    ///
    /// Results are ordered by hand id. Event hands are never returned here.
    /// A hand already running, paused, disabled, or out of retries is skipped.
    pub fn due_hands(&mut self, now_ms: u64) -> Vec<String> {
        let mut due = Vec::new();
        for (id, state) in &mut self.hands {
            if !is_dispatchable(state) {
                continue;
            }
            if matches!(state.spec.trigger, HandTrigger::EventTopic(_)) {
                continue;
            }
            if state.next_due_ms.is_some_and(|due_at| due_at <= now_ms) {
                state.status = HandStatus::Running;
                state.last_started_ms = Some(now_ms);
                due.push(id.clone());
            }
        }
        due
    }

    /// Publishes an event topic and returns the ids of hands it started.
    ///
    /// Every enabled, idle `EventTopic(topic)` hand is marked running and
    /// returned in id order. A hand already running or paused is skipped.
    pub fn trigger_event(&mut self, topic: &str, now_ms: u64) -> Vec<String> {
        let mut triggered = Vec::new();
        for (id, state) in &mut self.hands {
            if !is_dispatchable(state) {
                continue;
            }
            if !matches!(&state.spec.trigger, HandTrigger::EventTopic(t) if t == topic) {
                continue;
            }
            state.status = HandStatus::Running;
            state.last_started_ms = Some(now_ms);
            state.next_due_ms = None;
            triggered.push(id.clone());
        }
        triggered
    }

    /// Records a successful run and schedules the hand's next due time.
    ///
    /// Interval hands become due one interval after `now_ms`; one-shot and
    /// event hands are not scheduled again. The consecutive failure count is
    /// reset, so a later failure starts backoff from the base delay again.
    pub fn record_success(&mut self, id: &str, now_ms: u64) -> NaviResult<()> {
        let state = self
            .hands
            .get_mut(id)
            .ok_or_else(|| NaviError::NotFound(format!("hand '{id}' not found")))?;
        let attempt = state.retries.saturating_add(1);
        let started_at_ms = state.last_started_ms.unwrap_or(now_ms);
        state.history.push(HandRunRecord {
            hand_id: id.to_owned(),
            attempt,
            started_at_ms,
            finished_at_ms: now_ms,
            success: true,
            error: None,
        });
        let next_due_ms = match &state.spec.trigger {
            HandTrigger::IntervalMs(interval) => Some(now_ms.saturating_add(*interval)),
            HandTrigger::EventTopic(_) | HandTrigger::OneShotAtMs(_) => None,
        };
        state.status = HandStatus::Succeeded;
        state.retries = 0;
        state.next_due_ms = next_due_ms;
        state.last_started_ms = None;
        Ok(())
    }

    /// Records a failed run and schedules the next retry with exponential backoff.
    ///
    /// The delay after failure `n` is `backoff_ms * 2^(n - 1)`, saturating at
    /// `u64::MAX`. When `max_retries` retries have been used, the hand is left
    /// `Failed` with no further due time.
    pub fn record_failure(&mut self, id: &str, now_ms: u64) -> NaviResult<()> {
        self.record_failure_inner(id, None, now_ms)
    }

    /// Like [`Self::record_failure`], but stores a failure description.
    pub fn record_failure_with_error(
        &mut self,
        id: &str,
        error: impl Into<String>,
        now_ms: u64,
    ) -> NaviResult<()> {
        self.record_failure_inner(id, Some(error.into()), now_ms)
    }

    fn record_failure_inner(
        &mut self,
        id: &str,
        error: Option<String>,
        now_ms: u64,
    ) -> NaviResult<()> {
        let state = self
            .hands
            .get_mut(id)
            .ok_or_else(|| NaviError::NotFound(format!("hand '{id}' not found")))?;
        let retries = state.retries.saturating_add(1);
        let started_at_ms = state.last_started_ms.unwrap_or(now_ms);
        state.history.push(HandRunRecord {
            hand_id: id.to_owned(),
            attempt: retries,
            started_at_ms,
            finished_at_ms: now_ms,
            success: false,
            error,
        });
        state.retries = retries;
        state.status = HandStatus::Failed;
        state.next_due_ms = if retries <= state.spec.max_retries {
            let delay = state
                .spec
                .backoff_ms
                .saturating_mul(2u64.saturating_pow(retries.saturating_sub(1)));
            Some(now_ms.saturating_add(delay))
        } else {
            None
        };
        state.last_started_ms = None;
        Ok(())
    }

    /// Marks every running hand as idle so it can be dispatched after a restart.
    pub fn recover_running(&mut self) {
        for state in self.hands.values_mut() {
            if state.status == HandStatus::Running {
                state.status = HandStatus::Idle;
                state.last_started_ms = None;
            }
        }
    }

    /// Returns the specification of a hand, if registered.
    pub fn get(&self, id: &str) -> Option<&HandSpec> {
        self.hands.get(id).map(|state| &state.spec)
    }

    /// Returns the current status of a hand, if registered.
    pub fn status(&self, id: &str) -> Option<HandStatus> {
        self.hands.get(id).map(|state| state.status)
    }

    /// Returns the time at which a hand is next due, if scheduled.
    pub fn next_due_ms(&self, id: &str) -> Option<u64> {
        self.hands.get(id).and_then(|state| state.next_due_ms)
    }

    /// Returns the number of consecutive failures recorded for a hand.
    pub fn retries(&self, id: &str) -> Option<u32> {
        self.hands.get(id).map(|state| state.retries)
    }

    /// Returns the run history of a hand, oldest first.
    pub fn history(&self, id: &str) -> &[HandRunRecord] {
        self.hands
            .get(id)
            .map_or(&[], |state| state.history.as_slice())
    }

    /// Returns all registered hand ids in order.
    pub fn hand_ids(&self) -> Vec<String> {
        self.hands.keys().cloned().collect()
    }

    /// Returns the number of registered hands.
    pub fn len(&self) -> usize {
        self.hands.len()
    }

    /// Returns `true` when no hands are registered.
    pub fn is_empty(&self) -> bool {
        self.hands.is_empty()
    }

    /// Serializes the scheduler to a pretty-printed JSON string.
    pub fn to_json(&self) -> NaviResult<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Deserializes a scheduler from JSON.
    pub fn from_json(json: &str) -> NaviResult<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Writes the scheduler state as JSON to `path`, creating parent directories.
    pub fn save(&self, path: impl AsRef<Path>) -> NaviResult<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.to_json()?)?;
        Ok(())
    }

    /// Loads scheduler state from a JSON file.
    pub fn load(path: impl AsRef<Path>) -> NaviResult<Self> {
        let bytes = fs::read(path.as_ref())?;
        Self::from_json(&String::from_utf8_lossy(&bytes))
    }

    /// Replaces this scheduler with the state stored at `path`.
    pub fn load_into(&mut self, path: impl AsRef<Path>) -> NaviResult<()> {
        *self = Self::load(path)?;
        Ok(())
    }
}

/// Whether a hand may be dispatched right now.
fn is_dispatchable(state: &HandState) -> bool {
    state.spec.enabled && state.status != HandStatus::Running && state.status != HandStatus::Paused
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempState {
        dir: PathBuf,
    }

    impl TempState {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "navi-hands-scheduler-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self { dir }
        }

        fn file(&self) -> PathBuf {
            self.dir.join("nested").join("hands.json")
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn spec(id: &str, trigger: HandTrigger) -> HandSpec {
        HandSpec {
            id: id.to_owned(),
            name: format!("hand {id}"),
            trigger,
            max_retries: 2,
            backoff_ms: 100,
            enabled: true,
        }
    }

    #[test]
    fn register_rejects_blank_and_duplicate_ids() {
        let mut scheduler = HandScheduler::new();
        assert!(scheduler.is_empty());
        assert!(
            scheduler
                .register(spec("  ", HandTrigger::IntervalMs(10)))
                .is_err_and(|err| matches!(err, NaviError::InvalidInput(_)))
        );

        scheduler
            .register(spec("scan", HandTrigger::IntervalMs(10)))
            .expect("register scan");
        let duplicate = scheduler
            .register(spec("scan", HandTrigger::IntervalMs(20)))
            .expect_err("duplicate id rejected");
        assert!(matches!(duplicate, NaviError::InvalidInput(_)));
        assert_eq!(scheduler.len(), 1);
        assert_eq!(scheduler.status("scan"), Some(HandStatus::Idle));
        assert_eq!(
            scheduler.get("scan").map(|s| s.name.as_str()),
            Some("hand scan")
        );
        assert_eq!(scheduler.hand_ids(), ["scan"]);
    }

    #[test]
    fn interval_hands_are_due_immediately_and_after_success() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("poll", HandTrigger::IntervalMs(1_000)))
            .expect("register");
        assert_eq!(scheduler.due_hands(0), ["poll"]);
        assert_eq!(scheduler.status("poll"), Some(HandStatus::Running));
        assert!(scheduler.due_hands(5_000).is_empty());

        scheduler
            .record_success("poll", 10_000)
            .expect("record success");
        assert_eq!(scheduler.status("poll"), Some(HandStatus::Succeeded));
        assert_eq!(scheduler.next_due_ms("poll"), Some(11_000));
        assert!(scheduler.due_hands(10_999).is_empty());
        assert_eq!(scheduler.due_hands(11_000), ["poll"]);
        assert_eq!(scheduler.history("poll").len(), 1);
        assert!(scheduler.history("poll")[0].success);
        assert_eq!(scheduler.history("poll")[0].attempt, 1);
        assert_eq!(scheduler.history("poll")[0].started_at_ms, 0);
        assert_eq!(scheduler.history("poll")[0].finished_at_ms, 10_000);
    }

    #[test]
    fn one_shot_hand_runs_only_once() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("alarm", HandTrigger::OneShotAtMs(5_000)))
            .expect("register");
        assert!(scheduler.due_hands(4_999).is_empty());
        assert_eq!(scheduler.due_hands(5_000), ["alarm"]);
        assert!(scheduler.due_hands(9_999).is_empty());
        scheduler
            .record_success("alarm", 5_010)
            .expect("record success");
        assert!(scheduler.due_hands(u64::MAX).is_empty());
        assert_eq!(scheduler.next_due_ms("alarm"), None);
    }

    #[test]
    fn event_hand_runs_only_on_matching_topic() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("mail", HandTrigger::EventTopic("inbox".to_owned())))
            .expect("register");
        scheduler
            .register(spec(
                "other",
                HandTrigger::EventTopic("calendar".to_owned()),
            ))
            .expect("register");
        assert!(scheduler.due_hands(u64::MAX).is_empty());
        assert!(scheduler.trigger_event("unknown", 100).is_empty());

        let triggered = scheduler.trigger_event("inbox", 100);
        assert_eq!(triggered, ["mail"]);
        assert_eq!(scheduler.status("mail"), Some(HandStatus::Running));
        assert!(scheduler.trigger_event("inbox", 200).is_empty());
        scheduler
            .record_success("mail", 300)
            .expect("record success");
        assert_eq!(scheduler.trigger_event("inbox", 400), ["mail"]);
        assert_eq!(scheduler.trigger_event("calendar", 500), ["other"]);
    }

    #[test]
    fn pause_resume_and_disabled_specs_control_dispatch() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("a", HandTrigger::IntervalMs(10)))
            .expect("register a");
        let mut disabled = spec("b", HandTrigger::IntervalMs(10));
        disabled.enabled = false;
        scheduler.register(disabled).expect("register b");

        assert_eq!(scheduler.status("b"), Some(HandStatus::Paused));
        assert_eq!(scheduler.due_hands(0), ["a"]);
        scheduler.pause("a").expect("pause a");
        assert!(scheduler.due_hands(10_000).is_empty());
        scheduler.pause("a").expect("pause is idempotent");
        scheduler.resume("a").expect("resume a");
        scheduler.resume("b").expect("resume b");
        assert_eq!(scheduler.due_hands(10_000), ["a", "b"]);
        assert!(
            scheduler
                .pause("missing")
                .is_err_and(|err| matches!(err, NaviError::NotFound(_)))
        );
        assert!(
            scheduler
                .resume("missing")
                .is_err_and(|err| matches!(err, NaviError::NotFound(_)))
        );
    }

    #[test]
    fn failure_uses_exponential_backoff_then_exhausts_retries() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("job", HandTrigger::IntervalMs(500)))
            .expect("register");

        assert_eq!(scheduler.due_hands(0), ["job"]);
        scheduler
            .record_failure_with_error("job", "network down", 1_000)
            .expect("first failure");
        assert_eq!(scheduler.status("job"), Some(HandStatus::Failed));
        assert_eq!(scheduler.next_due_ms("job"), Some(1_100));
        assert_eq!(scheduler.retries("job"), Some(1));

        assert!(scheduler.due_hands(1_099).is_empty());
        assert_eq!(scheduler.due_hands(1_100), ["job"]);
        scheduler
            .record_failure("job", 1_100)
            .expect("second failure");
        assert_eq!(scheduler.next_due_ms("job"), Some(1_300));
        assert_eq!(scheduler.retries("job"), Some(2));

        assert_eq!(scheduler.due_hands(1_300), ["job"]);
        scheduler
            .record_failure("job", 1_300)
            .expect("third failure");
        assert_eq!(scheduler.status("job"), Some(HandStatus::Failed));
        assert_eq!(scheduler.next_due_ms("job"), None);
        assert!(scheduler.due_hands(u64::MAX).is_empty());

        let history = scheduler.history("job");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].error.as_deref(), Some("network down"));
        assert_eq!(history[1].attempt, 2);
        assert!(!history[2].success);
        assert!(
            scheduler
                .record_success("absent", 1)
                .is_err_and(|err| matches!(err, NaviError::NotFound(_)))
        );
        assert!(
            scheduler
                .record_failure("absent", 1)
                .is_err_and(|err| matches!(err, NaviError::NotFound(_)))
        );
    }

    #[test]
    fn success_resets_backoff_and_saturating_arithmetic_is_safe() {
        let mut scheduler = HandScheduler::new();
        let mut spec = spec("job", HandTrigger::IntervalMs(0));
        spec.backoff_ms = u64::MAX;
        spec.max_retries = u64::MAX as u32;
        scheduler.register(spec).expect("register");

        scheduler.due_hands(0);
        scheduler.record_failure("job", 10).expect("failure");
        assert_eq!(scheduler.next_due_ms("job"), Some(u64::MAX));
        scheduler.due_hands(u64::MAX);
        scheduler.record_success("job", u64::MAX).expect("success");
        assert_eq!(scheduler.retries("job"), Some(0));
        assert_eq!(scheduler.next_due_ms("job"), Some(u64::MAX));

        scheduler.due_hands(u64::MAX);
        scheduler
            .record_failure("job", u64::MAX)
            .expect("failure after reset");
        assert_eq!(scheduler.next_due_ms("job"), Some(u64::MAX));
        assert_eq!(scheduler.history("job").len(), 3);
    }

    #[test]
    fn save_and_load_roundtrip_preserves_all_state() {
        let temp = TempState::new();
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("poll", HandTrigger::IntervalMs(250)))
            .expect("register poll");
        scheduler
            .register(spec("wake", HandTrigger::OneShotAtMs(7_000)))
            .expect("register wake");
        scheduler
            .register(spec("mail", HandTrigger::EventTopic("inbox".to_owned())))
            .expect("register mail");
        assert_eq!(scheduler.due_hands(0), ["poll"]);
        scheduler
            .record_failure_with_error("poll", "timeout", 500)
            .expect("record failure");
        scheduler.pause("mail").expect("pause mail");

        scheduler.save(temp.file()).expect("save");
        let loaded = HandScheduler::load(temp.file()).expect("load");
        assert_eq!(loaded, scheduler);
        assert_eq!(loaded.status("poll"), Some(HandStatus::Failed));
        assert_eq!(loaded.next_due_ms("poll"), Some(600));
        assert_eq!(loaded.history("poll")[0].error.as_deref(), Some("timeout"));
        assert_eq!(loaded.status("wake"), Some(HandStatus::Idle));
        assert_eq!(loaded.status("mail"), Some(HandStatus::Paused));

        let mut overwritten = HandScheduler::new();
        overwritten.load_into(temp.file()).expect("load into");
        assert_eq!(overwritten, loaded);
        assert!(HandScheduler::load(temp.dir.join("missing.json")).is_err());
    }

    #[test]
    fn json_wire_format_is_stable_and_rejects_malformed_input() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("job", HandTrigger::EventTopic("t".to_owned())))
            .expect("register");
        let value = serde_json::to_value(&scheduler).expect("to value");
        assert_eq!(value["hands"]["job"]["spec"]["trigger"]["event_topic"], "t");
        assert_eq!(value["hands"]["job"]["status"], "idle");

        let encoded = scheduler.to_json().expect("to json");
        assert_eq!(
            HandScheduler::from_json(&encoded).expect("from json"),
            scheduler
        );
        assert!(HandScheduler::from_json("{not json").is_err());
    }

    #[test]
    fn recover_running_marks_in_flight_hands_idle() {
        let mut scheduler = HandScheduler::new();
        scheduler
            .register(spec("poll", HandTrigger::IntervalMs(10)))
            .expect("register");
        assert_eq!(scheduler.due_hands(0), ["poll"]);
        scheduler.recover_running();
        assert_eq!(scheduler.status("poll"), Some(HandStatus::Idle));
        assert_eq!(scheduler.due_hands(50), ["poll"]);
    }

    #[test]
    fn spec_defaults_and_deterministic_dispatch_order() {
        let mut scheduler = HandScheduler::new();
        let defaults = HandSpec::new("zeta", "Zeta", HandTrigger::IntervalMs(5));
        assert_eq!(defaults.max_retries, 3);
        assert_eq!(defaults.backoff_ms, 1_000);
        assert!(defaults.enabled);
        scheduler.register(defaults).expect("register zeta");
        scheduler
            .register(spec("alpha", HandTrigger::IntervalMs(5)))
            .expect("register alpha");
        scheduler
            .register(spec("event", HandTrigger::EventTopic("ping".to_owned())))
            .expect("register event");

        assert_eq!(scheduler.len(), 3);
        assert!(!scheduler.is_empty());
        assert_eq!(scheduler.hand_ids(), ["alpha", "event", "zeta"]);
        assert_eq!(scheduler.due_hands(0), ["alpha", "zeta"]);
        assert_eq!(scheduler.trigger_event("ping", 1), ["event"]);
        assert_eq!(scheduler.next_due_ms("zeta"), Some(0));

        assert_eq!(scheduler.get("missing"), None);
        assert_eq!(scheduler.status("missing"), None);
        assert_eq!(scheduler.next_due_ms("missing"), None);
        assert_eq!(scheduler.retries("missing"), None);
        assert!(scheduler.history("missing").is_empty());
    }
}
