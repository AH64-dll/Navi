//! Core synchronous agent turn loop.
//!
//! A [`ModelClient`] is called once per turn with the full transcript. When a
//! turn requests tools, each call is executed by a [`ToolExecutor`], its result
//! is appended to the transcript, and the model is called again. Steering
//! messages are injected before the next model call. Every transition is
//! recorded as a [`LoopEvent`], so a caller receives a complete, ordered trace
//! of the run.

use std::collections::BTreeMap;

use navi_types::{
    Message, NaviResult, Role, StopReason, ToolCall, ToolDefinition, ToolResult, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Produces one assistant message for the current transcript.
pub trait ModelClient {
    /// Completes a single turn from the system prompt, transcript, and tool set.
    fn complete(
        &mut self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> NaviResult<Message>;
}

/// Executes a tool call requested by the model.
pub trait ToolExecutor {
    /// Executes `call` and returns its result, including failure details.
    fn execute(&mut self, call: &ToolCall) -> ToolResult;
}

/// One observable transition in an agent loop run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoopEvent {
    /// The run started.
    AgentStart,
    /// A model turn started. Turns are counted from one.
    TurnStart { turn: usize },
    /// A message is about to be appended to the transcript.
    MessageStart { message: Message },
    /// A message was appended to the transcript.
    MessageEnd { message: Message },
    /// A tool call is about to be executed.
    ToolExecutionStart { tool_call: ToolCall },
    /// A tool call finished, successfully or otherwise.
    ToolExecutionEnd {
        tool_call: ToolCall,
        result: ToolResult,
    },
    /// A steering message was injected ahead of the next model call.
    SteeringInjected { message: Message },
    /// A model turn finished.
    TurnEnd {
        turn: usize,
        tool_results: usize,
        stop_reason: StopReason,
    },
    /// The turn budget was exhausted while work remained.
    MaxTurnsReached { max_turns: usize },
    /// The run ended without a client error.
    AgentEnd { message_count: usize },
}

/// Bounds and context for [`run_agent_loop`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLoopConfig {
    /// Maximum number of model calls in one run.
    pub max_turns: usize,
    /// System prompt passed to every model call.
    pub system_prompt: String,
}

impl AgentLoopConfig {
    /// Creates a configuration with an explicit turn budget and system prompt.
    pub fn new(max_turns: usize, system_prompt: impl Into<String>) -> Self {
        Self {
            max_turns,
            system_prompt: system_prompt.into(),
        }
    }
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_turns: 10,
            system_prompt: String::new(),
        }
    }
}

/// Outcome of a successful agent loop run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLoopResult {
    /// Full transcript: initial messages, steering, tool results, and replies.
    pub messages: Vec<Message>,
    /// Complete, ordered trace of the run.
    pub events: Vec<LoopEvent>,
    /// Number of model calls made.
    pub turns: usize,
    /// Why the loop stopped.
    pub stop_reason: StopReason,
    /// Usage accumulated across assistant turns.
    pub usage: Usage,
}

impl AgentLoopResult {
    /// Returns the most recent assistant message, if the transcript has one.
    pub fn final_message(&self) -> Option<&Message> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
    }
}

/// Runs the agent loop until the model stops requesting tools, steering is
/// exhausted, the turn budget is reached, or the client fails.
///
/// A client error is propagated to the caller. Tool failures are not fatal:
/// they are recorded in the trace and in the tool result message so the model
/// can decide how to proceed on the next turn.
pub fn run_agent_loop<C, E>(
    client: &mut C,
    executor: &mut E,
    config: &AgentLoopConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
    steering: &[Message],
) -> NaviResult<AgentLoopResult>
where
    C: ModelClient,
    E: ToolExecutor,
{
    let mut events = Vec::new();
    let mut transcript = Vec::with_capacity(messages.len() + steering.len());
    events.push(LoopEvent::AgentStart);
    for message in messages {
        append_message(&mut events, &mut transcript, message);
    }

    let mut steering_index = 0usize;
    let mut turns = 0usize;
    let mut usage = Usage::default();
    let mut stop_reason = StopReason::Stop;
    let mut capped = false;
    let mut needs_followup = false;

    loop {
        if turns >= config.max_turns {
            capped = true;
            if needs_followup {
                stop_reason = StopReason::ToolUse;
            }
            break;
        }

        turns += 1;
        events.push(LoopEvent::TurnStart { turn: turns });

        while steering_index < steering.len() {
            let injected = &steering[steering_index];
            steering_index += 1;
            events.push(LoopEvent::SteeringInjected {
                message: injected.clone(),
            });
            append_message(&mut events, &mut transcript, injected);
        }

        let assistant = client.complete(&config.system_prompt, &transcript, tools)?;
        if let Some(turn_usage) = &assistant.usage {
            usage.add_assign(turn_usage);
        }
        append_message(&mut events, &mut transcript, &assistant);

        let tool_calls: Vec<ToolCall> = assistant.tool_calls().into_iter().cloned().collect();
        let turn_stop = assistant.stop_reason.unwrap_or(StopReason::Stop);
        let terminal = matches!(
            assistant.stop_reason,
            Some(StopReason::Error | StopReason::Aborted)
        );

        // A terminal stop reason ends the run before any requested tool runs.
        let mut executed = 0usize;
        if !terminal {
            for call in &tool_calls {
                events.push(LoopEvent::ToolExecutionStart {
                    tool_call: call.clone(),
                });
                let result = executor.execute(call);
                events.push(LoopEvent::ToolExecutionEnd {
                    tool_call: call.clone(),
                    result: result.clone(),
                });
                let result_message = tool_result_message(&result, assistant.timestamp_ms);
                append_message(&mut events, &mut transcript, &result_message);
                executed += 1;
            }
        }

        events.push(LoopEvent::TurnEnd {
            turn: turns,
            tool_results: executed,
            stop_reason: turn_stop,
        });

        if terminal {
            stop_reason = turn_stop;
            break;
        }

        needs_followup = !tool_calls.is_empty();
        if !needs_followup {
            stop_reason = turn_stop;
        }
        if !needs_followup && steering_index >= steering.len() {
            break;
        }
    }

    if capped {
        events.push(LoopEvent::MaxTurnsReached {
            max_turns: config.max_turns,
        });
    }
    events.push(LoopEvent::AgentEnd {
        message_count: transcript.len(),
    });

    Ok(AgentLoopResult {
        messages: transcript,
        events,
        turns,
        stop_reason,
        usage,
    })
}

/// Emits the start/end pair for a message and appends it to the transcript.
fn append_message(events: &mut Vec<LoopEvent>, transcript: &mut Vec<Message>, message: &Message) {
    let owned = message.clone();
    events.push(LoopEvent::MessageStart {
        message: owned.clone(),
    });
    events.push(LoopEvent::MessageEnd {
        message: owned.clone(),
    });
    transcript.push(owned);
}

/// Converts a tool result into the transcript message consumed by the model.
fn tool_result_message(result: &ToolResult, timestamp_ms: u64) -> Message {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "tool_call_id".to_string(),
        Value::String(result.tool_call_id.clone()),
    );
    metadata.insert(
        "tool_name".to_string(),
        Value::String(result.tool_name.clone()),
    );
    metadata.insert("is_error".to_string(), Value::Bool(result.is_error));
    if let Some(details) = &result.details {
        metadata.insert("details".to_string(), details.clone());
    }
    Message {
        role: Role::ToolResult,
        content: result.content.clone(),
        timestamp_ms,
        stop_reason: None,
        usage: None,
        metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_types::{ContentBlock, NaviError};
    use serde_json::{Value, json};
    use std::collections::VecDeque;

    fn user(text: &str) -> Message {
        Message::user_text(text, 1)
    }

    fn assistant(content: Vec<ContentBlock>, stop: StopReason) -> Message {
        Message {
            role: Role::Assistant,
            content,
            timestamp_ms: 2,
            stop_reason: Some(stop),
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    fn assistant_text(text: &str, stop: StopReason) -> Message {
        assistant(vec![ContentBlock::Text { text: text.into() }], stop)
    }

    fn call(id: &str, name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: json!({ "path": path }),
        }
    }

    fn assistant_with_calls(calls: Vec<ToolCall>, stop: StopReason) -> Message {
        assistant(
            calls.into_iter().map(ContentBlock::ToolCall).collect(),
            stop,
        )
    }

    fn tool_result(call: &ToolCall, text: &str, is_error: bool) -> ToolResult {
        ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![ContentBlock::Text { text: text.into() }],
            is_error,
            details: None,
        }
    }

    struct ScriptedClient {
        responses: VecDeque<NaviResult<Message>>,
        calls: usize,
        system_prompts: Vec<String>,
        transcript_lengths: Vec<usize>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<NaviResult<Message>>) -> Self {
            Self {
                responses: responses.into(),
                calls: 0,
                system_prompts: Vec::new(),
                transcript_lengths: Vec::new(),
            }
        }
    }

    impl ModelClient for ScriptedClient {
        fn complete(
            &mut self,
            system: &str,
            messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> NaviResult<Message> {
            self.calls += 1;
            self.system_prompts.push(system.to_string());
            self.transcript_lengths.push(messages.len());
            self.responses
                .pop_front()
                .unwrap_or_else(|| Err(NaviError::Internal("no scripted response left".into())))
        }
    }

    struct ScriptedExecutor {
        calls: Vec<ToolCall>,
        results: VecDeque<ToolResult>,
    }

    impl ScriptedExecutor {
        fn new(results: Vec<ToolResult>) -> Self {
            Self {
                calls: Vec::new(),
                results: results.into(),
            }
        }
    }

    impl ToolExecutor for ScriptedExecutor {
        fn execute(&mut self, call: &ToolCall) -> ToolResult {
            self.calls.push(call.clone());
            self.results
                .pop_front()
                .unwrap_or_else(|| tool_result(call, "no scripted result", true))
        }
    }

    #[test]
    fn single_turn_produces_full_trace() {
        let mut client = ScriptedClient::new(vec![Ok(assistant_text("done", StopReason::Stop))]);
        let mut executor = ScriptedExecutor::new(Vec::new());
        let config = AgentLoopConfig::new(4, "system");

        let result = run_agent_loop(
            &mut client,
            &mut executor,
            &config,
            &[user("hello")],
            &[],
            &[],
        )
        .expect("loop succeeds");

        assert_eq!(client.calls, 1);
        assert_eq!(client.system_prompts, vec!["system"]);
        assert_eq!(result.turns, 1);
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].text_content(), "hello");
        assert_eq!(result.messages[1].text_content(), "done");
        assert!(executor.calls.is_empty());
        assert!(matches!(result.events.first(), Some(LoopEvent::AgentStart)));
        assert!(matches!(
            result.events.last(),
            Some(LoopEvent::AgentEnd { message_count: 2 })
        ));
        assert_eq!(
            result
                .events
                .iter()
                .filter(|event| matches!(event, LoopEvent::TurnStart { turn: 1 }))
                .count(),
            1
        );
    }

    #[test]
    fn tool_calls_are_executed_and_fed_back_to_model() {
        let tool = call("c1", "read", "Cargo.toml");
        let mut client = ScriptedClient::new(vec![
            Ok(assistant_with_calls(
                vec![tool.clone()],
                StopReason::ToolUse,
            )),
            Ok(assistant_text("all done", StopReason::Stop)),
        ]);
        let mut executor = ScriptedExecutor::new(vec![tool_result(&tool, "contents", false)]);
        let config = AgentLoopConfig::new(5, "sys");

        let result = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect("loop succeeds");

        assert_eq!(client.calls, 2);
        assert_eq!(client.transcript_lengths, vec![1, 3]);
        assert_eq!(executor.calls, vec![tool.clone()]);
        assert_eq!(result.turns, 2);
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.messages[2].role, Role::ToolResult);
        assert_eq!(result.messages[2].text_content(), "contents");
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(
            result
                .events
                .iter()
                .filter(|event| matches!(event, LoopEvent::ToolExecutionStart { .. }))
                .count(),
            1
        );
        assert!(result.events.iter().any(|event| matches!(
            event,
            LoopEvent::ToolExecutionEnd { tool_call, .. } if tool_call.id == "c1"
        )));
    }

    #[test]
    fn max_turns_caps_repeated_tool_calls() {
        let tool = call("c1", "loop", "x");
        let responses: Vec<NaviResult<Message>> = (0..5)
            .map(|_| {
                Ok(assistant_with_calls(
                    vec![tool.clone()],
                    StopReason::ToolUse,
                ))
            })
            .collect();
        let results: Vec<ToolResult> = (0..5).map(|_| tool_result(&tool, "again", false)).collect();
        let mut client = ScriptedClient::new(responses);
        let mut executor = ScriptedExecutor::new(results);
        let config = AgentLoopConfig::new(2, "sys");

        let result = run_agent_loop(
            &mut client,
            &mut executor,
            &config,
            &[user("start")],
            &[],
            &[],
        )
        .expect("loop succeeds");

        assert_eq!(result.turns, 2);
        assert_eq!(client.calls, 2);
        assert_eq!(executor.calls.len(), 2);
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, LoopEvent::MaxTurnsReached { max_turns: 2 }))
        );
    }

    #[test]
    fn steering_messages_are_injected_before_the_next_model_call() {
        let tool = call("c1", "read", "a");
        let mut client = ScriptedClient::new(vec![
            Ok(assistant_with_calls(
                vec![tool.clone()],
                StopReason::ToolUse,
            )),
            Ok(assistant_text("handled", StopReason::Stop)),
        ]);
        let mut executor = ScriptedExecutor::new(vec![tool_result(&tool, "data", false)]);
        let steering = user("steering now");
        let config = AgentLoopConfig::new(5, "sys");

        let result = run_agent_loop(
            &mut client,
            &mut executor,
            &config,
            &[user("go")],
            &[],
            std::slice::from_ref(&steering),
        )
        .expect("loop succeeds");

        assert_eq!(client.transcript_lengths, vec![2, 4]);
        assert_eq!(result.messages.len(), 5);
        assert_eq!(result.messages[1], steering);
        assert_eq!(result.messages[2].role, Role::Assistant);
        assert_eq!(result.messages[3].role, Role::ToolResult);
        assert_eq!(result.messages[4].text_content(), "handled");
        assert_eq!(
            result
                .events
                .iter()
                .filter(|event| matches!(event, LoopEvent::SteeringInjected { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn model_errors_propagate_to_caller() {
        let mut client = ScriptedClient::new(vec![Err(NaviError::Provider("boom".into()))]);
        let mut executor = ScriptedExecutor::new(Vec::new());
        let config = AgentLoopConfig::default();

        let error = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect_err("client failure must propagate");

        assert_eq!(error, NaviError::Provider("boom".into()));
        assert_eq!(client.calls, 1);
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn terminal_stop_reason_ends_loop_before_tool_execution() {
        let tool = call("c1", "read", "a");
        let mut client = ScriptedClient::new(vec![Ok(assistant_with_calls(
            vec![tool],
            StopReason::Aborted,
        ))]);
        let mut executor = ScriptedExecutor::new(Vec::new());
        let config = AgentLoopConfig::new(5, "sys");

        let result = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect("loop succeeds");

        assert_eq!(result.turns, 1);
        assert_eq!(result.stop_reason, StopReason::Aborted);
        assert!(executor.calls.is_empty());
        assert_eq!(result.messages.len(), 2);
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, LoopEvent::ToolExecutionStart { .. }))
        );
    }

    #[test]
    fn zero_turn_budget_never_calls_the_model() {
        struct PanickingClient;

        impl ModelClient for PanickingClient {
            fn complete(
                &mut self,
                _system: &str,
                _messages: &[Message],
                _tools: &[ToolDefinition],
            ) -> NaviResult<Message> {
                panic!("model must not be called with a zero turn budget")
            }
        }

        let mut client = PanickingClient;
        let mut executor = ScriptedExecutor::new(Vec::new());
        let config = AgentLoopConfig::new(0, "sys");

        let result = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect("loop succeeds");

        assert_eq!(result.turns, 0);
        assert_eq!(result.messages.len(), 1);
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, LoopEvent::MaxTurnsReached { max_turns: 0 }))
        );
    }

    #[test]
    fn tool_errors_are_recorded_without_aborting_the_loop() {
        let tool = call("c1", "read", "a");
        let mut client = ScriptedClient::new(vec![
            Ok(assistant_with_calls(
                vec![tool.clone()],
                StopReason::ToolUse,
            )),
            Ok(assistant_text("recovered", StopReason::Stop)),
        ]);
        let mut executor =
            ScriptedExecutor::new(vec![tool_result(&tool, "permission denied", true)]);
        let config = AgentLoopConfig::new(4, "sys");

        let result = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect("loop succeeds");

        assert_eq!(result.turns, 2);
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(
            result.messages[2].metadata.get("is_error"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            result
                .events
                .iter()
                .filter(|event| matches!(
                    event,
                    LoopEvent::ToolExecutionEnd { result, .. } if result.is_error
                ))
                .count(),
            1
        );
    }

    #[test]
    fn multiple_tool_calls_execute_in_order() {
        let first = call("c1", "read", "a");
        let second = call("c2", "write", "b");
        let mut client = ScriptedClient::new(vec![
            Ok(assistant_with_calls(
                vec![first.clone(), second.clone()],
                StopReason::ToolUse,
            )),
            Ok(assistant_text("ok", StopReason::Stop)),
        ]);
        let mut executor = ScriptedExecutor::new(vec![
            tool_result(&first, "one", false),
            tool_result(&second, "two", false),
        ]);
        let config = AgentLoopConfig::new(4, "sys");

        let result = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect("loop succeeds");

        assert_eq!(executor.calls, vec![first, second]);
        assert_eq!(result.messages[2].text_content(), "one");
        assert_eq!(result.messages[3].text_content(), "two");
        let turn_one_results = result.events.iter().find_map(|event| match event {
            LoopEvent::TurnEnd {
                turn: 1,
                tool_results,
                ..
            } => Some(*tool_results),
            _ => None,
        });
        assert_eq!(turn_one_results, Some(2));
    }

    #[test]
    fn usage_is_accumulated_and_final_message_is_last_assistant() {
        let tool = call("c1", "read", "a");
        let mut first = assistant_with_calls(vec![tool.clone()], StopReason::ToolUse);
        first.usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        });
        let mut second = assistant_text("done", StopReason::Stop);
        second.usage = Some(Usage {
            input_tokens: 20,
            output_tokens: 7,
            total_tokens: 27,
            cost_usd: 0.01,
            ..Usage::default()
        });
        let mut client = ScriptedClient::new(vec![Ok(first), Ok(second)]);
        let mut executor = ScriptedExecutor::new(vec![tool_result(&tool, "x", false)]);
        let config = AgentLoopConfig::new(4, "sys");

        let result = run_agent_loop(&mut client, &mut executor, &config, &[user("go")], &[], &[])
            .expect("loop succeeds");

        assert_eq!(result.usage.input_tokens, 30);
        assert_eq!(result.usage.output_tokens, 12);
        assert_eq!(result.usage.total_tokens, 42);
        assert!((result.usage.cost_usd - 0.01).abs() < 1e-12);
        assert_eq!(
            result.final_message().map(Message::text_content).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn events_round_trip_through_serde() {
        let events = vec![
            LoopEvent::AgentStart,
            LoopEvent::TurnStart { turn: 1 },
            LoopEvent::MessageStart {
                message: user("hi"),
            },
            LoopEvent::ToolExecutionStart {
                tool_call: call("c", "t", "a"),
            },
            LoopEvent::TurnEnd {
                turn: 1,
                tool_results: 0,
                stop_reason: StopReason::Stop,
            },
            LoopEvent::AgentEnd { message_count: 1 },
        ];

        let encoded = serde_json::to_string(&events).expect("serialize events");
        let decoded: Vec<LoopEvent> = serde_json::from_str(&encoded).expect("deserialize events");
        assert_eq!(decoded, events);
    }
}
