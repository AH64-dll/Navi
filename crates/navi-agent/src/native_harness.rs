//! Native agent harness: wires the agent loop, built-in tools, session store,
//! compaction, system prompts, the HTTP model client, and sandbox auditing into
//! one runnable unit.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use navi_ai::http_client::{LlmEndpointConfig, LlmHttpClient};
use navi_sandbox::audit_log::AuditLog;
use navi_sandbox::secret_guard::redact_secrets;
use navi_types::{ContentBlock, Message, NaviError, NaviResult, ToolDefinition, ToolResult};
use serde_json::{Value, json};

use crate::agent_loop::{self, AgentLoopConfig, AgentLoopResult, LoopEvent, ModelClient};
use crate::compaction::{self, CompactionConfig, CompactionPlan};
use crate::prompt::{self, PromptContext};
use crate::session_store::{SessionEntry, SessionEntryKind, SessionStore};
use crate::tool_bash::BashToolArgs;
use crate::tool_edit::EditToolArgs;
use crate::tool_read::ReadToolArgs;
use crate::tool_router::{ToolHandler, ToolRouter};
use crate::tool_write::WriteToolArgs;

/// Compaction thresholds applied to the session transcript before each turn.
const DEFAULT_COMPACTION: CompactionConfig = CompactionConfig {
    max_context_tokens: 4096,
    reserve_tokens: 512,
    keep_recent_tokens: 1024,
};

/// Longest single string written to one audit-log field.
const AUDIT_FIELD_CHARS: usize = 512;

/// Configuration for a [`NativeAgentHarness`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeHarnessConfig {
    /// Root directory that every built-in file tool is confined to.
    pub workspace_root: PathBuf,
    /// Path of the append-only JSONL session transcript.
    pub session_path: PathBuf,
    /// Path of the hash-chained JSONL audit log.
    pub audit_log_path: PathBuf,
    /// Base text of the assembled system prompt.
    pub system_prompt: String,
    /// Maximum model calls in one run of the agent loop.
    pub max_turns: usize,
}

impl Default for NativeHarnessConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::new(),
            session_path: PathBuf::new(),
            audit_log_path: PathBuf::new(),
            system_prompt: String::new(),
            max_turns: 10,
        }
    }
}

/// Assembles the four built-in tools into a router.
///
/// `read`, `write`, `edit`, and `bash` are registered with JSON schemas; the
/// router validates required fields before calling each tool's handler.
pub fn build_default_tool_router(workspace_root: PathBuf) -> ToolRouter {
    let mut router = ToolRouter::new();
    router.register(ReadHandler {
        workspace_root: workspace_root.clone(),
    });
    router.register(WriteHandler {
        workspace_root: workspace_root.clone(),
    });
    router.register(EditHandler {
        workspace_root: workspace_root.clone(),
    });
    router.register(BashHandler { workspace_root });
    router
}

/// A configured HTTP LLM client that can drive [`agent_loop::run_agent_loop`].
#[derive(Debug, Clone)]
pub struct HttpModelClient {
    client: LlmHttpClient,
}

impl HttpModelClient {
    /// Wraps a client built from `config`.
    pub fn new(config: LlmEndpointConfig) -> Self {
        Self {
            client: LlmHttpClient::new(config),
        }
    }

    /// Wraps an already constructed HTTP client.
    pub fn from_client(client: LlmHttpClient) -> Self {
        Self { client }
    }

    /// Returns the wrapped HTTP client.
    pub fn client(&self) -> &LlmHttpClient {
        &self.client
    }

    /// Consumes the adapter and returns the wrapped HTTP client.
    pub fn into_inner(self) -> LlmHttpClient {
        self.client
    }
}

impl Default for HttpModelClient {
    fn default() -> Self {
        Self::new(LlmEndpointConfig::default())
    }
}

impl From<LlmHttpClient> for HttpModelClient {
    fn from(client: LlmHttpClient) -> Self {
        Self::from_client(client)
    }
}

impl ModelClient for HttpModelClient {
    fn complete(
        &mut self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> NaviResult<Message> {
        self.client.complete(Some(system), messages, tools)
    }
}

/// Runs real tool calls, persists the transcript, and records an audit trail.
pub struct NativeAgentHarness {
    config: NativeHarnessConfig,
    router: ToolRouter,
    session: SessionStore,
    audit: AuditLog,
    entry_seq: u64,
    compaction: CompactionConfig,
}

impl NativeAgentHarness {
    /// Opens (or creates) the workspace, session, and audit log described by `config`.
    pub fn new(config: NativeHarnessConfig) -> NaviResult<Self> {
        fs::create_dir_all(&config.workspace_root)?;
        let session = SessionStore::open(&config.session_path)?;
        let audit = AuditLog::open(&config.audit_log_path)?;
        let entry_seq = u64::try_from(session.entries().len()).unwrap_or(u64::MAX);
        Ok(Self {
            router: build_default_tool_router(config.workspace_root.clone()),
            config,
            session,
            audit,
            entry_seq,
            compaction: DEFAULT_COMPACTION,
        })
    }

    /// Returns the harness configuration.
    pub fn config(&self) -> &NativeHarnessConfig {
        &self.config
    }

    /// Returns the router used to execute tool calls.
    pub fn router(&self) -> &ToolRouter {
        &self.router
    }

    /// Returns the session store backing this harness.
    pub fn session(&self) -> &SessionStore {
        &self.session
    }

    /// Returns the audit log backing this harness.
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// Returns the compaction thresholds used before each turn.
    pub fn compaction_config(&self) -> CompactionConfig {
        self.compaction
    }

    /// Replaces the compaction thresholds used before each turn.
    pub fn set_compaction_config(&mut self, config: CompactionConfig) {
        self.compaction = config;
    }

    /// Runs one turn with an HTTP-backed model client.
    pub fn run_turn_with_http_client(
        &mut self,
        client: &mut HttpModelClient,
        user_input: &str,
    ) -> NaviResult<AgentLoopResult> {
        self.run_turn_with_client(client, user_input)
    }

    /// Runs one turn: compact history, append the redacted user message, drive
    /// the model/tool loop, persist new messages, and audit every tool call.
    pub fn run_turn_with_client(
        &mut self,
        client: &mut dyn ModelClient,
        user_input: &str,
    ) -> NaviResult<AgentLoopResult> {
        let timestamp_ms = now_ms()?;
        let redacted_input = redact_secrets(user_input);
        self.audit.append_event(
            "user",
            "turn_start",
            self.config.session_path.to_string_lossy(),
            truncate_text(&redacted_input, AUDIT_FIELD_CHARS),
        )?;

        let mut messages = if self.session.entries().is_empty() {
            Vec::new()
        } else {
            self.session.linearize_current()?
        };
        let plan = compaction::plan_compaction(&messages, &self.compaction);
        if plan.should_compact {
            let summary = compaction_summary(&messages, &plan);
            messages = compaction::apply_compaction(&messages, &plan, &summary);
            self.audit.append_event(
                "native-harness",
                "compaction",
                self.config.session_path.to_string_lossy(),
                format!(
                    "dropped={};kept={};tokens={}",
                    plan.dropped_messages, plan.kept_messages, plan.estimated_tokens_before
                ),
            )?;
        }

        let user_message = Message::user_text(redacted_input, timestamp_ms);
        self.append_message_entry(&user_message)?;
        messages.push(user_message);

        let definitions = self.router.definitions();
        let system = prompt::assemble_system_prompt(&PromptContext {
            base_prompt: self.config.system_prompt.clone(),
            cwd: self.config.workspace_root.to_string_lossy().into_owned(),
            tools: definitions.iter().map(|tool| tool.name.clone()).collect(),
            ..PromptContext::default()
        });
        let config = AgentLoopConfig::new(self.config.max_turns, system);
        let mut adapter = ClientAdapter(client);
        let result = match agent_loop::run_agent_loop(
            &mut adapter,
            &mut self.router,
            &config,
            &messages,
            &definitions,
            &[],
        ) {
            Ok(result) => result,
            Err(error) => {
                let redacted = redact_secrets(&error.to_string());
                self.audit.append_event(
                    "native-harness",
                    "turn_error",
                    self.config.session_path.to_string_lossy(),
                    truncate_text(&redacted, AUDIT_FIELD_CHARS),
                )?;
                return Err(error);
            }
        };

        for message in result.messages.iter().skip(messages.len()) {
            self.append_message_entry(&redact_message(message)?)?;
        }
        for event in &result.events {
            let LoopEvent::ToolExecutionEnd {
                tool_call,
                result: tool_result,
            } = event
            else {
                continue;
            };
            let arguments =
                serde_json::to_string(&tool_call.arguments).unwrap_or_else(|_| "{}".to_owned());
            let target = format!("{} {arguments}", tool_call.name);
            self.audit.append_event(
                "model",
                format!("tool_{}", tool_call.name),
                truncate_text(&redact_secrets(&target), AUDIT_FIELD_CHARS),
                truncate_text(
                    &redact_secrets(&tool_result_text(tool_result)),
                    AUDIT_FIELD_CHARS,
                ),
            )?;
        }
        self.audit.append_event(
            "native-harness",
            "turn_end",
            self.config.session_path.to_string_lossy(),
            format!(
                "{:?};messages={}",
                result.stop_reason,
                result.messages.len()
            ),
        )?;

        Ok(result)
    }

    /// Appends one message as a new child of the current session leaf.
    fn append_message_entry(&mut self, message: &Message) -> NaviResult<String> {
        let mut id = format!("navi-{}", self.entry_seq);
        self.entry_seq = self.entry_seq.saturating_add(1);
        while self.session.get(&id).is_some() {
            id = format!("navi-{}", self.entry_seq);
            self.entry_seq = self.entry_seq.saturating_add(1);
        }
        self.session.append(SessionEntry::new(
            id,
            None,
            message.timestamp_ms,
            SessionEntryKind::Message {
                message: message.clone(),
            },
        ))
    }
}

/// Adapts a trait object to the sized generic accepted by the agent loop.
struct ClientAdapter<'a>(&'a mut dyn ModelClient);

impl ModelClient for ClientAdapter<'_> {
    fn complete(
        &mut self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> NaviResult<Message> {
        self.0.complete(system, messages, tools)
    }
}

/// Handler for the built-in `read` tool.
struct ReadHandler {
    workspace_root: PathBuf,
}

impl ToolHandler for ReadHandler {
    fn definition(&self) -> ToolDefinition {
        tool_definition(
            "read",
            "Read a UTF-8 text file in the workspace, optionally from a line range.",
            object_schema(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1},
                }),
                &["path"],
            ),
        )
    }

    fn execute(&self, args: &Value) -> NaviResult<String> {
        let args: ReadToolArgs = serde_json::from_value(args.clone())?;
        let output = crate::tool_read::execute_read(&self.workspace_root, &args)?;
        Ok(serde_json::to_string(&output)?)
    }
}

/// Handler for the built-in `write` tool.
struct WriteHandler {
    workspace_root: PathBuf,
}

impl ToolHandler for WriteHandler {
    fn definition(&self) -> ToolDefinition {
        tool_definition(
            "write",
            "Atomically write UTF-8 content to a file in the workspace.",
            object_schema(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "content": {"type": "string", "description": "Text to write"},
                }),
                &["path", "content"],
            ),
        )
    }

    fn execute(&self, args: &Value) -> NaviResult<String> {
        let args: WriteToolArgs = serde_json::from_value(args.clone())?;
        let output = crate::tool_write::execute_write(&self.workspace_root, &args)?;
        Ok(serde_json::to_string(&output)?)
    }
}

/// Handler for the built-in `edit` tool.
struct EditHandler {
    workspace_root: PathBuf,
}

impl ToolHandler for EditHandler {
    fn definition(&self) -> ToolDefinition {
        tool_definition(
            "edit",
            "Replace a unique exact text match in a workspace file and return a diff.",
            object_schema(
                json!({
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "old_text": {"type": "string", "description": "Exact text to replace"},
                    "new_text": {"type": "string", "description": "Replacement text"},
                }),
                &["path", "old_text", "new_text"],
            ),
        )
    }

    fn execute(&self, args: &Value) -> NaviResult<String> {
        let args: EditToolArgs = serde_json::from_value(args.clone())?;
        let output = crate::tool_edit::execute_edit(&self.workspace_root, &args)?;
        Ok(serde_json::to_string(&output)?)
    }
}

/// Handler for the built-in `bash` tool.
struct BashHandler {
    workspace_root: PathBuf,
}

impl ToolHandler for BashHandler {
    fn definition(&self) -> ToolDefinition {
        tool_definition(
            "bash",
            "Run a shell command in the workspace with a timeout and bounded output.",
            object_schema(
                json!({
                    "command": {"type": "string", "description": "Command passed to bash -lc"},
                    "timeout_ms": {"type": "integer", "minimum": 0},
                    "max_output_bytes": {"type": "integer", "minimum": 0},
                }),
                &["command"],
            ),
        )
    }

    fn execute(&self, args: &Value) -> NaviResult<String> {
        let args: BashToolArgs = serde_json::from_value(args.clone())?;
        let output = crate::tool_bash::execute_bash(&self.workspace_root, &args)?;
        Ok(serde_json::to_string(&output)?)
    }
}

/// Builds a tool definition with the common object-schema envelope.
fn tool_definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    }
}

/// Builds a JSON-schema object with the required fields listed.
fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

/// Returns the current wall-clock time in milliseconds, or an internal error.
fn now_ms() -> NaviResult<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| NaviError::Internal(format!("system clock error: {error}")))?;
    u64::try_from(elapsed.as_millis())
        .map_err(|_| NaviError::Internal("timestamp out of range".to_owned()))
}

/// Shortens text on a character boundary for use in an audit field.
fn truncate_text(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => {
            let mut shortened = text[..index].to_owned();
            shortened.push_str("...");
            shortened
        }
        None => text.to_owned(),
    }
}

/// Returns a copy of `message` with every text field redacted for persistence.
fn redact_message(message: &Message) -> NaviResult<Message> {
    let mut value = serde_json::to_value(message)?;
    redact_value(&mut value);
    Ok(serde_json::from_value(value)?)
}

/// Redacts every string value inside a JSON tree, leaving keys untouched.
fn redact_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact_secrets(text.as_str()),
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                redact_value(field);
            }
        }
        _ => {}
    }
}

/// Concatenates the text blocks of a tool result for auditing.
fn tool_result_text(result: &ToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Redacts and shortens the dropped prefix into a reusable checkpoint summary.
fn compaction_summary(messages: &[Message], plan: &CompactionPlan) -> String {
    let mut points = Vec::new();
    for message in messages.iter().take(plan.cut_index.min(messages.len())) {
        let text = message.text_content();
        if text.is_empty() {
            continue;
        }
        points.push(truncate_text(&redact_secrets(&text), 200));
        if points.len() == 8 {
            break;
        }
    }
    let detail = if points.is_empty() {
        String::new()
    } else {
        format!(" Key points: {}", points.join(" | "))
    };
    format!(
        "Compacted {} earlier messages (~{} tokens).{detail}",
        plan.dropped_messages, plan.estimated_tokens_before
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_ai::http_client::ProviderKind;
    use navi_sandbox::audit_log::GENESIS_HASH;
    use navi_sandbox::secret_guard::REDACTED;
    use navi_types::{Role, StopReason, ToolCall};
    use serde_json::json;
    use std::collections::{BTreeMap, VecDeque};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A deterministic model client that replays canned responses.
    struct ScriptedClient {
        responses: VecDeque<NaviResult<Message>>,
        calls: usize,
        transcripts: Vec<Vec<Message>>,
        systems: Vec<String>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<NaviResult<Message>>) -> Self {
            Self {
                responses: responses.into(),
                calls: 0,
                transcripts: Vec::new(),
                systems: Vec::new(),
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
            self.systems.push(system.to_owned());
            self.transcripts.push(messages.to_vec());
            self.responses
                .pop_front()
                .unwrap_or_else(|| Err(NaviError::Internal("no scripted response left".to_owned())))
        }
    }

    /// Unique workspace directory whose files are removed when the test ends.
    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "navi-native-harness-{label}-{}-{id}",
                std::process::id()
            ));
            if root.exists() {
                fs::remove_dir_all(&root).expect("remove stale workspace");
            }
            fs::create_dir_all(&root).expect("create workspace");
            Self { root }
        }

        fn config(&self, system_prompt: &str, max_turns: usize) -> NativeHarnessConfig {
            NativeHarnessConfig {
                workspace_root: self.root.clone(),
                session_path: self.root.join("session.jsonl"),
                audit_log_path: self.root.join("audit.jsonl"),
                system_prompt: system_prompt.to_owned(),
                max_turns,
            }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
        }
    }

    fn assistant_calls(calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: calls.into_iter().map(ContentBlock::ToolCall).collect(),
            timestamp_ms: 2,
            stop_reason: Some(StopReason::ToolUse),
            usage: None,
            metadata: BTreeMap::new(),
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message::assistant_text(text, 3)
    }

    fn bash_available() -> bool {
        Command::new("bash")
            .arg("-lc")
            .arg("exit 0")
            .status()
            .is_ok_and(|status| status.success())
    }

    fn audit_actions(harness: &NativeAgentHarness) -> Vec<String> {
        harness
            .audit()
            .records()
            .expect("read audit records")
            .into_iter()
            .map(|record| record.action)
            .collect()
    }

    #[test]
    fn default_router_registers_and_dispatches_builtin_tools() {
        let workspace = TempWorkspace::new("router");
        let router = build_default_tool_router(workspace.root.clone());
        let names: Vec<String> = router
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(names, vec!["bash", "edit", "read", "write"]);

        let written = router.dispatch(&call(
            "c1",
            "write",
            json!({"path": "a.txt", "content": "alpha beta"}),
        ));
        assert!(!written.is_error);
        let read = router.dispatch(&call("c2", "read", json!({"path": "a.txt"})));
        assert!(!read.is_error);
        assert!(tool_result_text(&read).contains("alpha beta"));
        let edited = router.dispatch(&call(
            "c3",
            "edit",
            json!({"path": "a.txt", "old_text": "alpha", "new_text": "ALPHA"}),
        ));
        assert!(!edited.is_error);
        assert_eq!(
            fs::read_to_string(workspace.root.join("a.txt")).expect("read edited file"),
            "ALPHA beta"
        );

        let unknown = router.dispatch(&call("c4", "missing", json!({})));
        assert!(unknown.is_error);
        assert!(tool_result_text(&unknown).contains("missing"));
        let missing_field = router.dispatch(&call("c5", "write", json!({"path": "b.txt"})));
        assert!(missing_field.is_error);
        assert!(tool_result_text(&missing_field).contains("content"));
    }

    #[test]
    fn run_turn_executes_write_read_edit_bash_and_verifies_audit_chain() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new("e2e");
        let mut harness =
            NativeAgentHarness::new(workspace.config("You are Navi.", 8)).expect("create harness");

        let mut client = ScriptedClient::new(vec![
            Ok(assistant_calls(vec![call(
                "c1",
                "write",
                json!({"path": "note.txt", "content": "hello Navi"}),
            )])),
            Ok(assistant_calls(vec![call(
                "c2",
                "read",
                json!({"path": "note.txt"}),
            )])),
            Ok(assistant_calls(vec![call(
                "c3",
                "edit",
                json!({"path": "note.txt", "old_text": "hello", "new_text": "hello there"}),
            )])),
            Ok(assistant_calls(vec![call(
                "c4",
                "bash",
                json!({"command": "cat note.txt", "timeout_ms": 1000, "max_output_bytes": 4096}),
            )])),
            Ok(assistant_text("done")),
        ]);

        let result = harness
            .run_turn_with_client(&mut client, "create a note")
            .expect("run turn");

        assert_eq!(client.calls, 5);
        assert_eq!(result.turns, 5);
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(
            fs::read_to_string(workspace.root.join("note.txt")).expect("read final note"),
            "hello there Navi"
        );
        assert_eq!(
            result
                .messages
                .iter()
                .filter(|message| message.role == Role::ToolResult)
                .count(),
            4
        );
        let bash_result = result
            .messages
            .iter()
            .find(|message| {
                message.metadata.get("tool_name").and_then(Value::as_str) == Some("bash")
            })
            .expect("bash tool result in transcript");
        assert!(bash_result.text_content().contains("hello there Navi"));
        assert!(client.systems[0].contains("You are Navi."));
        assert!(client.systems[0].contains("<cwd>"));

        let records = harness.audit().records().expect("read audit records");
        assert_eq!(records.len(), 6);
        assert_eq!(records[0].prev_hash, GENESIS_HASH);
        for pair in records.windows(2) {
            assert_eq!(pair[1].prev_hash, pair[0].hash);
        }
        let actions: Vec<&str> = records
            .iter()
            .map(|record| record.action.as_str())
            .collect();
        assert_eq!(
            actions,
            vec![
                "turn_start",
                "tool_write",
                "tool_read",
                "tool_edit",
                "tool_bash",
                "turn_end"
            ]
        );
        assert!(records[1].target.contains("note.txt"));
        assert_eq!(
            harness.audit().verify().expect("verify chain"),
            records.len()
        );
    }

    #[test]
    fn run_turn_reuses_persisted_session_on_next_turn() {
        let workspace = TempWorkspace::new("session");
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 4)).expect("harness");

        let mut first = ScriptedClient::new(vec![
            Ok(assistant_calls(vec![call(
                "c1",
                "write",
                json!({"path": "memo.txt", "content": "remember me"}),
            )])),
            Ok(assistant_text("saved")),
        ]);
        let first_result = harness
            .run_turn_with_client(&mut first, "remember this")
            .expect("first turn");
        assert_eq!(
            harness.session().entries().len(),
            first_result.messages.len()
        );

        let mut second = ScriptedClient::new(vec![Ok(assistant_text("recalled"))]);
        let second_result = harness
            .run_turn_with_client(&mut second, "what did I say?")
            .expect("second turn");
        let transcript = &second.transcripts[0];
        assert_eq!(transcript.len(), first_result.messages.len() + 1);
        assert!(transcript.iter().any(|message| {
            message.role == Role::User && message.text_content() == "remember this"
        }));
        assert!(transcript.iter().any(|message| {
            message.role == Role::Assistant && message.text_content() == "saved"
        }));
        assert_eq!(
            second_result
                .final_message()
                .map(Message::text_content)
                .as_deref(),
            Some("recalled")
        );
        assert_eq!(
            harness.session().entries().len(),
            first_result.messages.len() + 2
        );
        assert!(harness.session().leaf_id().is_some());
    }

    #[test]
    fn run_turn_redacts_secrets_in_session_and_audit_log() {
        let workspace = TempWorkspace::new("redact");
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 4)).expect("harness");
        let mut client = ScriptedClient::new(vec![
            Ok(assistant_calls(vec![call(
                "c1",
                "write",
                json!({"path": "secret.txt", "content": "token ghp_abcdef123456"}),
            )])),
            Ok(assistant_text("ok")),
        ]);

        harness
            .run_turn_with_client(&mut client, "key is sk-abcdef123456")
            .expect("run turn");

        let session = fs::read_to_string(workspace.config("sys", 4).session_path).expect("session");
        let audit = fs::read_to_string(workspace.config("sys", 4).audit_log_path).expect("audit");
        for secret in ["sk-abcdef123456", "ghp_abcdef123456"] {
            assert!(!session.contains(secret), "session leaked {secret}");
            assert!(!audit.contains(secret), "audit leaked {secret}");
        }
        assert!(session.contains(REDACTED));
        assert!(audit.contains(REDACTED));
        assert_eq!(harness.audit().verify().expect("verify chain"), 3);
    }

    #[test]
    fn client_error_is_audited_and_propagated() {
        let workspace = TempWorkspace::new("error");
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 4)).expect("harness");
        let mut client = ScriptedClient::new(vec![Err(NaviError::Provider(
            "boom with sk-abcdef123456".to_owned(),
        ))]);

        let error = harness
            .run_turn_with_client(&mut client, "go")
            .expect_err("client failure propagates");
        assert!(matches!(error, NaviError::Provider(_)));
        assert_eq!(client.calls, 1);
        assert_eq!(audit_actions(&harness), vec!["turn_start", "turn_error"]);
        let audit = fs::read_to_string(workspace.config("sys", 4).audit_log_path).expect("audit");
        assert!(!audit.contains("sk-abcdef123456"));
        assert_eq!(harness.session().entries().len(), 1);
        assert_eq!(harness.audit().verify().expect("verify chain"), 2);
    }

    #[test]
    fn zero_turn_budget_does_not_call_the_model() {
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

        let workspace = TempWorkspace::new("zero");
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 0)).expect("harness");
        let result = harness
            .run_turn_with_client(&mut PanickingClient, "hi")
            .expect("run turn");

        assert_eq!(result.turns, 0);
        assert!(
            result
                .messages
                .iter()
                .any(|message| { message.role == Role::User && message.text_content() == "hi" })
        );
        assert_eq!(audit_actions(&harness), vec!["turn_start", "turn_end"]);
        assert_eq!(harness.session().entries().len(), 1);
    }

    #[test]
    fn compaction_applies_to_a_large_persisted_history() {
        let workspace = TempWorkspace::new("compact");
        let session_path = workspace.root.join("session.jsonl");
        {
            let mut store = SessionStore::open(&session_path).expect("open session");
            for index in 0..12u64 {
                store
                    .append(SessionEntry::new(
                        format!("seed-{index}"),
                        None,
                        index,
                        SessionEntryKind::Message {
                            message: Message::user_text("x".repeat(2000), index),
                        },
                    ))
                    .expect("append seed message");
            }
        }
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 4)).expect("harness");
        let mut client = ScriptedClient::new(vec![Ok(assistant_text("short"))]);

        let result = harness
            .run_turn_with_client(&mut client, "new question")
            .expect("run turn");

        let transcript = &client.transcripts[0];
        assert!(transcript[0].text_content().starts_with("Compacted "));
        assert!(transcript.len() < 13);
        assert_eq!(result.messages.len(), transcript.len() + 1);
        assert!(audit_actions(&harness).contains(&"compaction".to_owned()));
        assert_eq!(harness.audit().verify().expect("verify chain"), 3);
    }

    #[test]
    fn audit_chain_detects_tampering_after_a_run() {
        let workspace = TempWorkspace::new("tamper");
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 2)).expect("harness");
        let mut client = ScriptedClient::new(vec![Ok(assistant_text("done"))]);
        harness
            .run_turn_with_client(&mut client, "hello")
            .expect("run turn");

        let path = workspace.root.join("audit.jsonl");
        let raw = fs::read_to_string(&path).expect("read audit log");
        let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
        let mut record: navi_sandbox::audit_log::AuditRecord =
            serde_json::from_str(&lines[1]).expect("parse audit record");
        record.outcome = "tampered".to_owned();
        lines[1] = serde_json::to_string(&record).expect("serialize record");
        fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write audit log");

        assert!(navi_sandbox::audit_log::verify_chain(&path).is_err());
        assert!(harness.audit().verify().is_err());
    }

    #[test]
    fn http_client_adapter_reports_invalid_endpoint_through_the_harness() {
        let workspace = TempWorkspace::new("http");
        let mut harness = NativeAgentHarness::new(workspace.config("sys", 2)).expect("harness");
        let client = LlmHttpClient::new(LlmEndpointConfig::new(
            ProviderKind::OpenAiCompat,
            "   ",
            "key",
            "model",
        ));
        let mut adapter = HttpModelClient::from(client);

        let error = harness
            .run_turn_with_http_client(&mut adapter, "hello")
            .expect_err("empty base URL is rejected");
        assert!(matches!(error, NaviError::InvalidInput(_)));
        assert_eq!(adapter.client().config().model, "model");
        assert_eq!(audit_actions(&harness), vec!["turn_start", "turn_error"]);
    }
}
