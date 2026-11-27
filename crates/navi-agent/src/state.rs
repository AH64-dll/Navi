//! Agent state machine for `navi-agent`.
//!
//! Owns the conversation transcript, pending steering/follow-up queues, the
//! lifecycle phase, the abort latch, active tool-call ids, and cumulative usage.
//! The API mirrors the mutable state held by the `Agent` class in
//! `packages/agent/src/agent.ts`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use navi_types::{ContentBlock, Message, ModelRef, NaviError, NaviResult, Role, SessionId, Usage};
use serde::{Deserialize, Serialize};

/// Lifecycle phase of an agent run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    /// No run is in progress.
    #[default]
    Idle,
    /// A model response is being streamed.
    Streaming,
    /// One or more tool calls are executing.
    ExecutingTools,
    /// The active run was cancelled by an abort request.
    Aborted,
    /// The active run failed.
    Error,
}

/// Policy used when draining a pending-message queue.
///
/// `All` drains every queued message at a drain point; `OneAtATime` drains only
/// the oldest message and leaves the rest for later drain points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringMode {
    /// Drain every queued message.
    All,
    /// Drain only the oldest queued message.
    #[default]
    OneAtATime,
}

/// Static configuration captured when an [`AgentState`] is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Model used for subsequent turns.
    pub model: ModelRef,
    /// Initial system prompt, stored as the leading system message.
    pub system_prompt: String,
    /// Drain policy for steering messages.
    pub steering_mode: SteeringMode,
    /// Drain policy for follow-up messages.
    pub follow_up_mode: SteeringMode,
    /// Optional session identifier forwarded to providers.
    pub session_id: Option<SessionId>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: ModelRef::new("unknown", "unknown"),
            system_prompt: String::new(),
            steering_mode: SteeringMode::default(),
            follow_up_mode: SteeringMode::default(),
            session_id: None,
        }
    }
}

/// FIFO queue of pending messages with a configurable drain policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingQueue {
    mode: SteeringMode,
    messages: VecDeque<Message>,
}

impl PendingQueue {
    /// Creates an empty queue using `mode` as the drain policy.
    pub fn new(mode: SteeringMode) -> Self {
        Self {
            mode,
            messages: VecDeque::new(),
        }
    }

    /// Returns the current drain policy.
    pub fn mode(&self) -> SteeringMode {
        self.mode
    }

    /// Replaces the drain policy without changing queued messages.
    pub fn set_mode(&mut self, mode: SteeringMode) {
        self.mode = mode;
    }

    /// Appends a message to the back of the queue.
    pub fn enqueue(&mut self, message: Message) {
        self.messages.push_back(message);
    }

    /// Returns `true` when at least one message is queued.
    pub fn has_items(&self) -> bool {
        !self.is_empty()
    }

    /// Returns `true` when the queue holds no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns the number of queued messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns how many messages the current drain policy selects.
    fn selected_count(&self) -> usize {
        match self.mode {
            SteeringMode::All => self.messages.len(),
            SteeringMode::OneAtATime => usize::from(!self.messages.is_empty()),
        }
    }

    /// Previews the messages that the next [`Self::drain`] would return.
    pub fn peek(&self) -> Vec<&Message> {
        self.messages.iter().take(self.selected_count()).collect()
    }

    /// Removes and returns the messages selected by the drain policy.
    pub fn drain(&mut self) -> Vec<Message> {
        let count = self.selected_count();
        self.messages.drain(..count).collect()
    }

    /// Removes every queued message.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

/// Mutable state of an agent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    config: AgentConfig,
    phase: AgentPhase,
    abort_requested: bool,
    messages: Vec<Message>,
    streaming_message: Option<Message>,
    pending_tool_calls: BTreeSet<String>,
    steering: PendingQueue,
    followup: PendingQueue,
    usage: Usage,
    error_message: Option<String>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new(AgentConfig::default())
    }
}

impl AgentState {
    /// Creates state from `config` and seeds the transcript with its system message.
    pub fn new(config: AgentConfig) -> Self {
        Self {
            messages: vec![Self::system_message(&config.system_prompt)],
            phase: AgentPhase::Idle,
            abort_requested: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            steering: PendingQueue::new(config.steering_mode),
            followup: PendingQueue::new(config.follow_up_mode),
            usage: Usage::default(),
            error_message: None,
            config,
        }
    }

    /// Builds the leading system message for a prompt.
    fn system_message(prompt: &str) -> Message {
        Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
            timestamp_ms: 0,
            stop_reason: None,
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Returns the configuration captured at construction time.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Replaces the model used for future turns.
    pub fn set_model(&mut self, model: ModelRef) {
        self.config.model = model;
    }

    /// Replaces the session identifier used for future turns.
    pub fn set_session_id(&mut self, session_id: Option<SessionId>) {
        self.config.session_id = session_id;
    }

    /// Returns the current lifecycle phase.
    pub fn phase(&self) -> AgentPhase {
        self.phase
    }

    /// Starts a run from an idle, aborted, or failed phase.
    ///
    /// Runtime-only fields from the previous run are cleared. Returns an error
    /// when a run is already live.
    pub fn begin_run(&mut self) -> NaviResult<()> {
        match self.phase {
            AgentPhase::Idle | AgentPhase::Aborted | AgentPhase::Error => {
                self.phase = AgentPhase::Streaming;
                self.abort_requested = false;
                self.streaming_message = None;
                self.error_message = None;
                self.pending_tool_calls.clear();
                Ok(())
            }
            AgentPhase::Streaming | AgentPhase::ExecutingTools => {
                let phase = self.phase;
                Err(NaviError::Internal(format!(
                    "cannot start a run while phase is {phase:?}"
                )))
            }
        }
    }

    /// Enters tool execution from the streaming phase.
    pub fn begin_tool_execution(&mut self) -> NaviResult<()> {
        if self.phase == AgentPhase::Streaming {
            self.phase = AgentPhase::ExecutingTools;
            Ok(())
        } else {
            let phase = self.phase;
            Err(NaviError::Internal(format!(
                "cannot execute tools while phase is {phase:?}"
            )))
        }
    }

    /// Returns from tool execution to the streaming phase.
    pub fn end_tool_execution(&mut self) -> NaviResult<()> {
        if self.phase == AgentPhase::ExecutingTools {
            self.phase = AgentPhase::Streaming;
            Ok(())
        } else {
            let phase = self.phase;
            Err(NaviError::Internal(format!(
                "cannot end tool execution while phase is {phase:?}"
            )))
        }
    }

    /// Latches an abort request and aborts a live run if one is active.
    ///
    /// Returns `true` only when the request moved a live run into `Aborted`.
    pub fn request_abort(&mut self) -> bool {
        let was_live = matches!(
            self.phase,
            AgentPhase::Streaming | AgentPhase::ExecutingTools
        );
        self.abort_requested = true;
        if was_live {
            self.phase = AgentPhase::Aborted;
        }
        was_live
    }

    /// Returns `true` while an abort request is latched.
    pub fn is_abort_requested(&self) -> bool {
        self.abort_requested
    }

    /// Clears a latched abort request without changing the phase.
    pub fn clear_abort(&mut self) {
        self.abort_requested = false;
    }

    /// Records a terminal error and moves the run into `Error`.
    pub fn mark_error(&mut self, message: impl Into<String>) {
        self.error_message = Some(message.into());
        self.phase = AgentPhase::Error;
        self.streaming_message = None;
        self.pending_tool_calls.clear();
    }

    /// Completes the run and returns the state to `Idle`.
    ///
    /// The error message, if any, is retained until the next [`Self::begin_run`].
    pub fn finish_run(&mut self) {
        self.phase = AgentPhase::Idle;
        self.abort_requested = false;
        self.streaming_message = None;
        self.pending_tool_calls.clear();
    }

    /// Returns the transcript, including the leading system message.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Appends a message and folds its reported usage into the cumulative total.
    pub fn append_message(&mut self, message: Message) {
        if let Some(usage) = &message.usage {
            self.usage.add_assign(usage);
        }
        self.messages.push(message);
    }

    /// Appends a plain-text user message with the supplied timestamp.
    pub fn append_user_text(&mut self, text: impl Into<String>, timestamp_ms: u64) {
        self.append_message(Message::user_text(text, timestamp_ms));
    }

    /// Replaces the transcript without re-accounting usage.
    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Removes every non-system message, keeping the first system message as baseline.
    pub fn clear_history(&mut self) {
        let baseline = self
            .messages
            .iter()
            .find(|message| message.role == Role::System)
            .cloned();
        self.messages = match baseline {
            Some(message) => vec![message],
            None => vec![Self::system_message(&self.config.system_prompt)],
        };
    }

    /// Returns the prompt from the leading system message, or the configured default.
    pub fn system_prompt(&self) -> &str {
        self.messages
            .first()
            .filter(|message| message.role == Role::System)
            .and_then(|message| message.content.first())
            .map_or(self.config.system_prompt.as_str(), |block| match block {
                ContentBlock::Text { text } => text.as_str(),
                _ => self.config.system_prompt.as_str(),
            })
    }

    /// Updates the configured prompt and the leading system message together.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        self.config.system_prompt = prompt.clone();
        let message = Self::system_message(&prompt);
        let leading_is_system = self
            .messages
            .first()
            .is_some_and(|message| message.role == Role::System);
        if leading_is_system {
            self.messages[0] = message;
        } else {
            self.messages.insert(0, message);
        }
    }

    /// Returns the steering queue drain policy.
    pub fn steering_mode(&self) -> SteeringMode {
        self.steering.mode()
    }

    /// Updates the steering queue drain policy in place.
    pub fn set_steering_mode(&mut self, mode: SteeringMode) {
        self.config.steering_mode = mode;
        self.steering.set_mode(mode);
    }

    /// Queues a steering message for injection after the current turn.
    pub fn enqueue_steering(&mut self, message: Message) {
        self.steering.enqueue(message);
    }

    /// Removes and returns the steering messages selected by the drain policy.
    pub fn dequeue_steering(&mut self) -> Vec<Message> {
        self.steering.drain()
    }

    /// Previews the next steering messages without consuming them.
    pub fn peek_steering(&self) -> Vec<&Message> {
        self.steering.peek()
    }

    /// Returns `true` when steering messages are pending.
    pub fn has_steering(&self) -> bool {
        self.steering.has_items()
    }

    /// Removes all steering messages.
    pub fn clear_steering(&mut self) {
        self.steering.clear();
    }

    /// Returns the follow-up queue drain policy.
    pub fn follow_up_mode(&self) -> SteeringMode {
        self.followup.mode()
    }

    /// Updates the follow-up queue drain policy in place.
    pub fn set_follow_up_mode(&mut self, mode: SteeringMode) {
        self.config.follow_up_mode = mode;
        self.followup.set_mode(mode);
    }

    /// Queues a message to run only after the agent would otherwise stop.
    pub fn enqueue_followup(&mut self, message: Message) {
        self.followup.enqueue(message);
    }

    /// Removes and returns the follow-up messages selected by the drain policy.
    pub fn dequeue_followup(&mut self) -> Vec<Message> {
        self.followup.drain()
    }

    /// Previews the next follow-up messages without consuming them.
    pub fn peek_followup(&self) -> Vec<&Message> {
        self.followup.peek()
    }

    /// Returns `true` when follow-up messages are pending.
    pub fn has_followup(&self) -> bool {
        self.followup.has_items()
    }

    /// Removes all follow-up messages.
    pub fn clear_followup(&mut self) {
        self.followup.clear();
    }

    /// Returns `true` when either queue still holds pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.has_steering() || self.has_followup()
    }

    /// Previews the messages selected for the next injection, steering first.
    ///
    /// Follow-up messages are only visible when no steering messages are
    /// pending, matching the precedence of the agent loop's drain points.
    pub fn peek_queued_messages(&self) -> Vec<&Message> {
        let steering = self.peek_steering();
        if steering.is_empty() {
            self.peek_followup()
        } else {
            steering
        }
    }

    /// Removes all steering and follow-up messages.
    pub fn clear_all_queues(&mut self) {
        self.steering.clear();
        self.followup.clear();
    }

    /// Registers a tool call as active.
    ///
    /// Returns an error for a blank id or an id that is already active.
    pub fn begin_tool_call(&mut self, id: impl Into<String>) -> NaviResult<()> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(NaviError::InvalidInput(
                "tool call id must not be empty".to_owned(),
            ));
        }
        if self.pending_tool_calls.contains(&id) {
            return Err(NaviError::InvalidInput(format!(
                "tool call '{id}' is already active"
            )));
        }
        self.pending_tool_calls.insert(id);
        Ok(())
    }

    /// Marks a tool call complete; returns `true` when the id was active.
    pub fn finish_tool_call(&mut self, id: &str) -> bool {
        self.pending_tool_calls.remove(id)
    }

    /// Returns the ids of all active tool calls.
    pub fn pending_tool_calls(&self) -> &BTreeSet<String> {
        &self.pending_tool_calls
    }

    /// Returns `true` when at least one tool call is active.
    pub fn has_pending_tool_calls(&self) -> bool {
        !self.pending_tool_calls.is_empty()
    }

    /// Drops all active tool call ids.
    pub fn clear_pending_tool_calls(&mut self) {
        self.pending_tool_calls.clear();
    }

    /// Returns cumulative token and cost accounting for the session.
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Adds a usage record to the cumulative total.
    pub fn record_usage(&mut self, usage: &Usage) {
        self.usage.add_assign(usage);
    }

    /// Resets cumulative usage to zero.
    pub fn reset_usage(&mut self) {
        self.usage = Usage::default();
    }

    /// Returns the in-progress assistant message, if any.
    pub fn streaming_message(&self) -> Option<&Message> {
        self.streaming_message.as_ref()
    }

    /// Sets or clears the in-progress assistant message.
    pub fn set_streaming_message(&mut self, message: Option<Message>) {
        self.streaming_message = message;
    }

    /// Returns the most recent error message, if any.
    pub fn error_message(&self) -> Option<&str> {
        self.error_message.as_deref()
    }

    /// Sets or clears the most recent error message.
    pub fn set_error_message(&mut self, message: Option<String>) {
        self.error_message = message;
    }

    /// Clears runtime state, queues, and usage while retaining the system baseline.
    pub fn reset(&mut self) {
        self.clear_history();
        self.clear_all_queues();
        self.clear_pending_tool_calls();
        self.streaming_message = None;
        self.error_message = None;
        self.abort_requested = false;
        self.phase = AgentPhase::Idle;
        self.reset_usage();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::StopReason;

    fn usage(input: u64, output: u64, cost: f64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cost_usd: cost,
            ..Usage::default()
        }
    }

    fn assistant_with_usage(text: &str, usage: Usage) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            timestamp_ms: 1,
            stop_reason: Some(StopReason::Stop),
            usage: Some(usage),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn initial_state_seeds_system_message_and_defaults() {
        let config = AgentConfig {
            model: ModelRef::new("openai", "gpt-test"),
            system_prompt: "be brief".to_owned(),
            session_id: Some(SessionId::new("s-1")),
            ..AgentConfig::default()
        };
        let state = AgentState::new(config.clone());
        assert_eq!(state.phase(), AgentPhase::Idle);
        assert_eq!(state.system_prompt(), "be brief");
        assert_eq!(state.messages().len(), 1);
        assert_eq!(state.messages()[0].role, Role::System);
        assert_eq!(state.config(), &config);
        assert!(!state.has_steering());
        assert!(!state.has_followup());
        assert!(!state.has_pending_tool_calls());
        assert!(!state.is_abort_requested());
        assert_eq!(state.usage(), &Usage::default());
        assert_eq!(state.steering_mode(), SteeringMode::OneAtATime);
        assert_eq!(state.follow_up_mode(), SteeringMode::OneAtATime);
    }

    #[test]
    fn append_message_accumulates_usage_in_order() {
        let mut state = AgentState::default();
        state.append_user_text("hello", 10);
        state.append_message(assistant_with_usage("hi", usage(100, 20, 0.25)));
        state.append_message(assistant_with_usage("again", usage(50, 10, 0.5)));

        assert_eq!(state.messages().len(), 4);
        assert_eq!(state.messages()[0].role, Role::System);
        assert_eq!(state.messages()[1].text_content(), "hello");
        assert_eq!(state.messages()[2].text_content(), "hi");
        assert_eq!(state.messages()[3].text_content(), "again");
        let total = state.usage();
        assert_eq!(total.input_tokens, 150);
        assert_eq!(total.output_tokens, 30);
        assert_eq!(total.total_tokens, 180);
        assert!((total.cost_usd - 0.75).abs() < 1e-12);

        state.reset_usage();
        assert_eq!(state.usage(), &Usage::default());
    }

    #[test]
    fn steering_queue_all_mode_drains_every_message() {
        let mut state = AgentState::default();
        state.set_steering_mode(SteeringMode::All);
        assert_eq!(state.steering_mode(), SteeringMode::All);
        for i in 0..3 {
            state.enqueue_steering(Message::user_text(format!("s{i}"), i));
        }
        assert!(state.has_steering());
        assert_eq!(state.peek_steering().len(), 3);
        let drained = state.dequeue_steering();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].text_content(), "s0");
        assert_eq!(drained[2].text_content(), "s2");
        assert!(!state.has_steering());
        assert!(state.dequeue_steering().is_empty());
    }

    #[test]
    fn steering_queue_one_at_a_time_drains_oldest() {
        let mut state = AgentState::default();
        state.enqueue_steering(Message::user_text("first", 1));
        state.enqueue_steering(Message::user_text("second", 2));
        state.enqueue_steering(Message::user_text("third", 3));
        assert_eq!(state.peek_steering().len(), 1);
        assert_eq!(state.peek_steering()[0].text_content(), "first");
        assert_eq!(state.dequeue_steering()[0].text_content(), "first");
        assert_eq!(state.dequeue_steering()[0].text_content(), "second");
        state.clear_steering();
        assert!(!state.has_steering());
        assert!(state.dequeue_steering().is_empty());
    }

    #[test]
    fn followup_queue_modes_and_bulk_helpers() {
        let mut state = AgentState::default();
        state.enqueue_followup(Message::user_text("f1", 1));
        state.enqueue_followup(Message::user_text("f2", 2));
        assert!(state.has_queued_messages());
        assert_eq!(state.dequeue_followup()[0].text_content(), "f1");
        assert!(state.has_followup());
        state.set_follow_up_mode(SteeringMode::All);
        assert_eq!(state.follow_up_mode(), SteeringMode::All);
        assert_eq!(state.peek_followup().len(), 1);
        state.clear_all_queues();
        assert!(!state.has_queued_messages());
        assert!(state.dequeue_followup().is_empty());
    }

    #[test]
    fn tool_call_tracking_rejects_duplicate_and_empty_ids() {
        let mut state = AgentState::default();
        assert!(state.begin_tool_call("call_1").is_ok());
        let duplicate = state
            .begin_tool_call("call_1")
            .expect_err("duplicate id rejected");
        assert!(matches!(duplicate, NaviError::InvalidInput(_)));
        let empty = state.begin_tool_call(" ").expect_err("blank id rejected");
        assert!(matches!(empty, NaviError::InvalidInput(_)));
        assert_eq!(state.pending_tool_calls().len(), 1);
        assert!(state.has_pending_tool_calls());
        assert!(state.finish_tool_call("call_1"));
        assert!(!state.finish_tool_call("call_1"));
        assert!(!state.has_pending_tool_calls());
        assert!(state.begin_tool_call("call_2").is_ok());
        state.clear_pending_tool_calls();
        assert!(state.pending_tool_calls().is_empty());
    }

    #[test]
    fn phase_transitions_and_abort_latch() {
        let mut state = AgentState::default();
        assert!(state.begin_run().is_ok());
        assert_eq!(state.phase(), AgentPhase::Streaming);
        assert!(state.begin_run().is_err());
        assert!(state.begin_tool_execution().is_ok());
        assert_eq!(state.phase(), AgentPhase::ExecutingTools);
        assert!(state.begin_tool_execution().is_err());
        assert!(state.end_tool_execution().is_ok());
        assert_eq!(state.phase(), AgentPhase::Streaming);

        assert!(state.request_abort());
        assert!(state.is_abort_requested());
        assert_eq!(state.phase(), AgentPhase::Aborted);
        assert!(!state.request_abort());
        state.clear_abort();
        assert!(!state.is_abort_requested());
        state.finish_run();
        assert_eq!(state.phase(), AgentPhase::Idle);

        assert!(state.begin_run().is_ok());
        state.mark_error("boom");
        assert_eq!(state.phase(), AgentPhase::Error);
        assert_eq!(state.error_message(), Some("boom"));
        assert!(state.begin_run().is_ok());
        assert_eq!(state.error_message(), None);
    }

    #[test]
    fn system_prompt_update_replaces_leading_message() {
        let mut state = AgentState::new(AgentConfig {
            system_prompt: "old".to_owned(),
            ..AgentConfig::default()
        });
        state.set_system_prompt("new");
        assert_eq!(state.system_prompt(), "new");
        assert_eq!(state.messages().len(), 1);
        assert_eq!(state.messages()[0].text_content(), "new");

        state.replace_messages(vec![Message::user_text("no system", 1)]);
        state.set_system_prompt("restored");
        assert_eq!(state.messages()[0].role, Role::System);
        assert_eq!(state.messages()[1].text_content(), "no system");
        assert_eq!(state.system_prompt(), "restored");
    }

    #[test]
    fn reset_clears_runtime_state_and_keeps_system_baseline() {
        let mut state = AgentState::new(AgentConfig {
            system_prompt: "baseline".to_owned(),
            ..AgentConfig::default()
        });
        state.append_user_text("hello", 1);
        state.append_message(assistant_with_usage("reply", usage(10, 5, 0.01)));
        state.enqueue_steering(Message::user_text("steer", 2));
        state.enqueue_followup(Message::user_text("follow", 3));
        assert!(state.begin_run().is_ok());
        assert!(state.begin_tool_execution().is_ok());
        assert!(state.begin_tool_call("t1").is_ok());
        state.set_streaming_message(Some(Message::assistant_text("partial", 4)));
        state.mark_error("bad");
        state.request_abort();

        state.reset();
        assert_eq!(state.phase(), AgentPhase::Idle);
        assert_eq!(state.messages().len(), 1);
        assert_eq!(state.messages()[0].text_content(), "baseline");
        assert!(!state.has_queued_messages());
        assert!(!state.has_pending_tool_calls());
        assert!(state.streaming_message().is_none());
        assert_eq!(state.error_message(), None);
        assert!(!state.is_abort_requested());
        assert_eq!(state.usage(), &Usage::default());
    }

    #[test]
    fn peek_queued_messages_prefers_steering_over_followup() {
        let mut state = AgentState::default();
        assert!(state.peek_queued_messages().is_empty());

        state.enqueue_followup(Message::user_text("follow", 1));
        assert_eq!(state.peek_queued_messages()[0].text_content(), "follow");

        state.enqueue_steering(Message::user_text("steer", 2));
        state.enqueue_steering(Message::user_text("steer2", 3));
        let peeked = state.peek_queued_messages();
        assert_eq!(peeked.len(), 1);
        assert_eq!(peeked[0].text_content(), "steer");
        assert!(state.has_followup());

        state.set_steering_mode(SteeringMode::All);
        assert_eq!(state.peek_queued_messages().len(), 2);
        assert_eq!(state.dequeue_steering().len(), 2);
        assert_eq!(state.peek_queued_messages()[0].text_content(), "follow");
        assert_eq!(state.dequeue_followup()[0].text_content(), "follow");
    }

    #[test]
    fn begin_run_clears_stale_runtime_state_after_abort_or_error() {
        let mut state = AgentState::default();
        assert!(state.begin_run().is_ok());
        assert!(state.begin_tool_call("stale").is_ok());
        state.set_streaming_message(Some(Message::assistant_text("partial", 1)));
        state.request_abort();
        assert_eq!(state.phase(), AgentPhase::Aborted);

        assert!(state.begin_run().is_ok());
        assert_eq!(state.phase(), AgentPhase::Streaming);
        assert!(!state.is_abort_requested());
        assert!(!state.has_pending_tool_calls());
        assert!(state.streaming_message().is_none());

        state.mark_error("failed");
        assert_eq!(state.phase(), AgentPhase::Error);
        assert_eq!(state.error_message(), Some("failed"));
        assert!(state.begin_run().is_ok());
        assert_eq!(state.phase(), AgentPhase::Streaming);
        assert_eq!(state.error_message(), None);
    }

    #[test]
    fn replace_messages_does_not_reaccount_usage() {
        let mut state = AgentState::default();
        state.append_message(assistant_with_usage("metered", usage(40, 8, 0.1)));
        assert_eq!(state.usage().total_tokens, 48);

        state.replace_messages(vec![
            Message::user_text("replaced", 1),
            assistant_with_usage("also metered", usage(100, 20, 0.2)),
        ]);
        assert_eq!(state.messages().len(), 2);
        assert_eq!(state.messages()[1].text_content(), "also metered");
        assert_eq!(state.usage().total_tokens, 48);

        state.record_usage(&usage(2, 1, 0.01));
        assert_eq!(state.usage().input_tokens, 42);
        assert_eq!(state.usage().output_tokens, 9);
    }

    #[test]
    fn serde_roundtrip_preserves_state() {
        let mut state = AgentState::new(AgentConfig {
            model: ModelRef::new("anthropic", "claude-test"),
            system_prompt: "system".to_owned(),
            session_id: Some(SessionId::new("session-9")),
            ..AgentConfig::default()
        });
        state.set_steering_mode(SteeringMode::All);
        state.append_user_text("hello", 42);
        state.enqueue_steering(Message::user_text("steer", 43));
        state.append_message(assistant_with_usage("reply", usage(12, 7, 0.125)));
        assert!(state.begin_tool_call("t1").is_ok());

        let encoded = serde_json::to_string(&state).expect("serialize state");
        let decoded: AgentState = serde_json::from_str(&encoded).expect("deserialize state");
        assert_eq!(decoded, state);
    }
}
