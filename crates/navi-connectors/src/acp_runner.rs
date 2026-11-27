//! Live subprocess runner for the Agent Client Protocol (ACP).
//!
//! [`run_acp_turn`] spawns an ACP-speaking executable, performs the
//! `initialize` / `session/new` / `session/prompt` handshake over newline
//! delimited JSON-RPC 2.0, normalizes `session/update` notifications through
//! [`crate::acp`], answers agent permission requests with the workspace
//! approval policy, and bounds the whole turn with a caller-supplied timeout.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use navi_types::{NaviError, NaviResult};
use serde_json::Value;

use crate::acp::{
    AcpJsonRpcError, AcpJsonRpcMessage, AcpJsonRpcRequest, AcpJsonRpcResponse,
    AcpPermissionDecision, AcpUpdateEvent, AcpUpdateNormalizer, build_initialize_request,
    build_session_new_request, build_session_prompt_request, evaluate_permission_policy,
    parse_acp_line, parse_permission_options, session_id_from_result,
};

/// Time granted to the agent to exit after its stdin closes before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

/// Configuration for one ACP turn driven by a subprocess.
#[derive(Debug, Clone)]
pub struct AcpRunConfig {
    /// Executable name or path that speaks ACP on stdin/stdout.
    pub executable: String,
    /// Command-line arguments passed to the executable.
    pub args: Vec<String>,
    /// Working directory for the child, or the current directory when absent.
    pub cwd: Option<PathBuf>,
    /// User text sent as the single text block of `session/prompt`.
    pub prompt: String,
    /// Total budget in milliseconds for the handshake and the prompt reply.
    pub timeout_ms: u64,
    /// Whether permission requests for read-only tools are approved.
    pub auto_approve_read_only: bool,
}

/// Result of one completed ACP turn.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpTurnOutcome {
    /// Session id advertised by the agent in `session/new`.
    pub session_id: String,
    /// Concatenated text of every `agent_message_chunk` delta.
    pub reply_text: String,
    /// Every normalized update observed during the turn, in arrival order.
    pub updates: Vec<AcpUpdateEvent>,
    /// Child process exit code, or `1` when it was terminated by a signal.
    pub exit_code: i32,
}

/// Runs one ACP turn against a freshly spawned agent process.
///
/// The child is always reaped before this function returns, including on
/// timeout or protocol errors. Update callbacks run on the calling thread.
pub fn run_acp_turn(
    cfg: &AcpRunConfig,
    on_update: &mut dyn FnMut(&AcpUpdateEvent),
) -> NaviResult<AcpTurnOutcome> {
    if cfg.executable.trim().is_empty() {
        return Err(NaviError::InvalidInput(
            "ACP executable must not be empty".to_owned(),
        ));
    }

    let cwd = match &cfg.cwd {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(NaviError::from)?,
    };

    let mut child = Command::new(&cfg.executable)
        .args(&cfg.args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| {
            NaviError::Transport(format!(
                "failed to spawn ACP agent '{}': {err}",
                cfg.executable
            ))
        })?;

    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err(NaviError::Transport(
            "ACP agent stdout was not piped".to_owned(),
        ));
    };
    let Some(stdin) = child.stdin.take() else {
        kill_and_reap(&mut child);
        return Err(NaviError::Transport(
            "ACP agent stdin was not piped".to_owned(),
        ));
    };

    let (sender, receiver) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let turn = perform_turn(stdin, receiver, on_update, cfg, &cwd);
    let exit_code = reap_after_turn(&mut child);
    let (session_id, reply_text, updates) = turn?;
    Ok(AcpTurnOutcome {
        session_id,
        reply_text,
        updates,
        exit_code,
    })
}

/// Drives the handshake and prompt, owning stdin until the turn ends.
fn perform_turn(
    stdin: ChildStdin,
    receiver: Receiver<String>,
    on_update: &mut dyn FnMut(&AcpUpdateEvent),
    cfg: &AcpRunConfig,
    cwd: &Path,
) -> NaviResult<(String, String, Vec<AcpUpdateEvent>)> {
    let mut session = AcpSession {
        stdin: Some(stdin),
        receiver,
        started: Instant::now(),
        timeout: Duration::from_millis(cfg.timeout_ms),
        normalizer: AcpUpdateNormalizer::new(),
        updates: Vec::new(),
        reply_text: String::new(),
        deferred: Vec::new(),
        auto_approve_read_only: cfg.auto_approve_read_only,
        on_update,
    };

    let initialize = build_initialize_request(1_u64);
    let _initialize_result = session.request(initialize)?;

    let new_session = build_session_new_request(2_u64, cwd);
    let new_result = session.request(new_session)?;
    let session_id = session_id_from_result(Some(&new_result)).ok_or_else(|| {
        NaviError::Protocol("ACP session/new reply did not contain a sessionId".to_owned())
    })?;

    let prompt = build_session_prompt_request(3_u64, &session_id, &cfg.prompt);
    let _prompt_result = session.request(prompt)?;

    Ok((session_id, session.reply_text, session.updates))
}

/// Mutable state for a single request/response exchange with the agent.
struct AcpSession<'a> {
    stdin: Option<ChildStdin>,
    receiver: Receiver<String>,
    started: Instant,
    timeout: Duration,
    normalizer: AcpUpdateNormalizer,
    updates: Vec<AcpUpdateEvent>,
    reply_text: String,
    deferred: Vec<AcpJsonRpcResponse>,
    auto_approve_read_only: bool,
    on_update: &'a mut dyn FnMut(&AcpUpdateEvent),
}

impl AcpSession<'_> {
    /// Sends a request and returns its successful result, servicing other frames meanwhile.
    fn request(&mut self, request: AcpJsonRpcRequest) -> NaviResult<Value> {
        let id = request
            .id
            .clone()
            .ok_or_else(|| NaviError::Internal("ACP request has no id".to_owned()))?;
        let line = request.to_line()?;
        self.send_line(&line)?;

        if let Some(index) = self.deferred.iter().position(|reply| reply.id == id) {
            let reply = self.deferred.remove(index);
            return reply.into_result();
        }

        loop {
            let line = self.next_line()?;
            match parse_acp_line(&line) {
                Ok(AcpJsonRpcMessage::Response(reply)) if reply.id == id => {
                    return reply.into_result();
                }
                Ok(AcpJsonRpcMessage::Response(reply)) => self.deferred.push(reply),
                Ok(AcpJsonRpcMessage::Notification(notification)) => {
                    self.handle_notification(&notification);
                }
                Ok(AcpJsonRpcMessage::Request(agent_request)) => {
                    self.handle_agent_request(&agent_request)?;
                }
                Err(_) => continue,
            }
        }
    }

    /// Reads one frame from stdout within the remaining turn budget.
    fn next_line(&self) -> NaviResult<String> {
        let remaining = self.timeout.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            return Err(timeout_error());
        }
        match self.receiver.recv_timeout(remaining) {
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => Err(timeout_error()),
            Err(RecvTimeoutError::Disconnected) => Err(NaviError::Transport(
                "ACP agent stdout closed before the turn finished".to_owned(),
            )),
        }
    }

    /// Writes one complete JSON-RPC frame to the agent.
    fn send_line(&mut self, line: &str) -> NaviResult<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| NaviError::Transport("ACP agent stdin is closed".to_owned()))?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|err| NaviError::Transport(format!("ACP stdin write failed: {err}")))?;
        stdin
            .flush()
            .map_err(|err| NaviError::Transport(format!("ACP stdin flush failed: {err}")))
    }

    /// Normalizes a `session/update` notification and notifies the caller.
    fn handle_notification(&mut self, notification: &AcpJsonRpcRequest) {
        if notification.method != "session/update" {
            return;
        }
        let events = self
            .normalizer
            .push_notification(&notification.method, &notification.params);
        for event in events {
            if let AcpUpdateEvent::Delta { text } = &event {
                self.reply_text.push_str(text);
            }
            (self.on_update)(&event);
            self.updates.push(event);
        }
    }

    /// Answers an agent-initiated request, applying the permission policy.
    fn handle_agent_request(&mut self, request: &AcpJsonRpcRequest) -> NaviResult<()> {
        let id = request
            .id
            .clone()
            .ok_or_else(|| NaviError::Protocol("ACP agent request has no id".to_owned()))?;
        let reply = if request.method == "session/request_permission" {
            let options = parse_permission_options(&request.params);
            let tool_call = request.params.get("toolCall");
            let decision = if self.auto_approve_read_only && is_read_only_tool_call(tool_call) {
                AcpPermissionDecision::Allow
            } else {
                AcpPermissionDecision::Deny
            };
            let evaluation = evaluate_permission_policy(decision, &options);
            AcpJsonRpcResponse::success(id, evaluation.reply_result())
        } else {
            AcpJsonRpcResponse::failure(id, AcpJsonRpcError::unsupported_method(&request.method))
        };
        let line = reply.to_line()?;
        self.send_line(&line)
    }
}

/// Reports whether a tool call only inspects state and can be auto-approved.
fn is_read_only_tool_call(tool_call: Option<&Value>) -> bool {
    const READ_VERBS: &[&str] = &[
        "read", "view", "search", "grep", "glob", "list", "find", "get", "show", "status",
        "inspect", "info", "cat", "ls",
    ];
    const WRITE_VERBS: &[&str] = &[
        "write", "edit", "delete", "remove", "create", "execute", "run", "bash", "shell", "patch",
        "move", "copy", "apply", "install",
    ];

    let Some(tool_call) = tool_call else {
        return false;
    };
    let mut joined = String::new();
    for key in ["kind", "title", "name"] {
        if let Some(text) = tool_call.get(key).and_then(Value::as_str) {
            joined.push(' ');
            joined.push_str(&text.to_ascii_lowercase());
        }
    }
    if joined.is_empty() {
        return false;
    }
    let words: Vec<&str> = joined
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has_write = words
        .iter()
        .any(|word| WRITE_VERBS.iter().any(|verb| word.starts_with(verb)));
    if has_write {
        return false;
    }
    words
        .iter()
        .any(|word| READ_VERBS.iter().any(|verb| word.starts_with(verb)))
}

fn timeout_error() -> NaviError {
    NaviError::Timeout("ACP turn timed out".to_owned())
}

/// Lets the agent exit after stdin closes, then kills and reaps it if needed.
fn reap_after_turn(child: &mut Child) -> i32 {
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                return child.wait().map(|s| s.code().unwrap_or(1)).unwrap_or(1);
            }
            Err(_) => {
                kill_and_reap(child);
                return 1;
            }
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC_SCRIPT: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-mock"}}'
      ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"Hello "}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"world"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      ;;
  esac
done
"#;

    const PERMISSION_SCRIPT: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-perm"}}'
      ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":77,"method":"session/request_permission","params":{"toolCall":{"kind":"TOOL_KIND","title":"TOOL_TITLE"},"options":TOOL_OPTIONS}}'
      IFS= read -r perm_reply
      case "$perm_reply" in
        *'"id":77'*'"optionId":"allow-1"'*) text="approved" ;;
        *'"id":77'*'"outcome":"cancelled"'*) text="cancelled" ;;
        *'"id":77'*) text="denied" ;;
        *) text="bad-id" ;;
      esac
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"%s"}}}}\n' "$text"
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      ;;
  esac
done
"#;

    fn config(script: &str, timeout_ms: u64, auto_approve_read_only: bool) -> AcpRunConfig {
        AcpRunConfig {
            executable: "sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            cwd: None,
            prompt: "hello".to_owned(),
            timeout_ms,
            auto_approve_read_only,
        }
    }

    fn run(script: &str, timeout_ms: u64, auto_approve: bool) -> NaviResult<AcpTurnOutcome> {
        let cfg = config(script, timeout_ms, auto_approve);
        run_acp_turn(&cfg, &mut |_event| {})
    }

    fn permission_script(kind: &str, title: &str, options: &str) -> String {
        PERMISSION_SCRIPT
            .replace("TOOL_KIND", kind)
            .replace("TOOL_TITLE", title)
            .replace("TOOL_OPTIONS", options)
    }

    const BOTH_OPTIONS: &str = r#"[{"optionId":"allow-1","kind":"allow_once"},{"optionId":"reject-1","kind":"reject_once"}]"#;

    #[test]
    fn normal_turn_collects_reply_updates_and_exit_code() {
        let cfg = config(BASIC_SCRIPT, 5_000, false);
        let mut seen = Vec::new();
        let outcome = run_acp_turn(&cfg, &mut |event: &AcpUpdateEvent| seen.push(event.clone()))
            .expect("mock ACP turn succeeds");

        assert_eq!(outcome.session_id, "sess-mock");
        assert_eq!(outcome.reply_text, "Hello world");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.updates.len(), 2);
        assert_eq!(seen, outcome.updates);
        assert!(matches!(outcome.updates[0], AcpUpdateEvent::Delta { .. }));
    }

    #[test]
    fn auto_approve_accepts_read_only_permission_request() {
        let script = permission_script("read", "Read file", BOTH_OPTIONS);
        let outcome = run(&script, 5_000, true).expect("approved turn succeeds");
        assert_eq!(outcome.session_id, "sess-perm");
        assert_eq!(outcome.reply_text, "approved");
    }

    #[test]
    fn permission_requests_are_denied_without_auto_approval() {
        let script = permission_script("read", "Read file", BOTH_OPTIONS);
        let outcome = run(&script, 5_000, false).expect("denied turn succeeds");
        assert_eq!(outcome.reply_text, "denied");
    }

    #[test]
    fn mutating_tool_is_denied_even_with_auto_approval() {
        let script = permission_script("write", "Write file", BOTH_OPTIONS);
        let outcome = run(&script, 5_000, true).expect("denied turn succeeds");
        assert_eq!(outcome.reply_text, "denied");
    }

    #[test]
    fn approval_not_offered_is_downgraded_to_cancelled() {
        let script = permission_script(
            "read",
            "Read file",
            r#"[{"optionId":"reject-1","kind":"reject_once"}]"#,
        );
        let outcome = run(&script, 5_000, true).expect("cancelled turn succeeds");
        assert_eq!(outcome.reply_text, "cancelled");
    }

    #[test]
    fn timeout_is_enforced_when_agent_never_replies() {
        let err = run("sleep 5", 100, false).expect_err("silent agent times out");
        assert!(matches!(err, NaviError::Timeout(_)));
    }

    #[test]
    fn malformed_frames_and_unknown_updates_are_ignored() {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' 'not json'
      printf '%s\n' '[1,2,3]'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"unknown_kind"}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-noisy"}}'
      ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"text":"quiet"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      ;;
  esac
done
"#;
        let outcome = run(script, 5_000, false).expect("noisy mock still succeeds");
        assert_eq!(outcome.session_id, "sess-noisy");
        assert_eq!(outcome.reply_text, "quiet");
        assert_eq!(outcome.updates.len(), 1);
    }

    #[test]
    fn error_reply_from_agent_is_reported_as_protocol_error() {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"unsupported"}}'
      ;;
  esac
done
"#;
        let err = run(script, 5_000, false).expect_err("agent error is propagated");
        assert!(matches!(err, NaviError::Protocol(_)));
        assert!(err.to_string().contains("unsupported"));
    }

    #[test]
    fn missing_session_id_is_a_protocol_error() {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
      ;;
  esac
done
"#;
        let err = run(script, 5_000, false).expect_err("missing sessionId is rejected");
        assert!(matches!(err, NaviError::Protocol(_)));
    }

    #[test]
    fn tool_updates_are_forwarded_and_collected() {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-tool"}}'
      ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read file","rawInput":{"path":"a.txt"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"content":{"text":"ok"}}]}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      ;;
  esac
done
"#;
        let cfg = config(script, 5_000, false);
        let mut seen = Vec::new();
        let outcome = run_acp_turn(&cfg, &mut |event: &AcpUpdateEvent| seen.push(event.clone()))
            .expect("tool turn succeeds");
        assert_eq!(outcome.session_id, "sess-tool");
        assert_eq!(outcome.reply_text, "");
        assert_eq!(outcome.updates.len(), 2);
        assert_eq!(seen, outcome.updates);
    }

    #[test]
    fn read_only_tool_detection_ignores_noise_and_write_verbs() {
        assert!(is_read_only_tool_call(Some(&serde_json::json!({
            "kind": "VIEW_FILE",
            "title": "read-only inspect"
        }))));
        assert!(!is_read_only_tool_call(Some(&serde_json::json!({
            "kind": "write",
            "title": "Read file"
        }))));
        assert!(!is_read_only_tool_call(Some(&serde_json::json!({
            "rawInput": {"path": "a.txt"}
        }))));
        assert!(!is_read_only_tool_call(None));
    }

    #[test]
    fn spawn_failure_and_empty_executable_are_reported() {
        let cfg = AcpRunConfig {
            executable: "/definitely/not/a/navi-acp-agent".to_owned(),
            args: Vec::new(),
            cwd: None,
            prompt: "hello".to_owned(),
            timeout_ms: 1_000,
            auto_approve_read_only: false,
        };
        assert!(matches!(
            run_acp_turn(&cfg, &mut |_event| {}),
            Err(NaviError::Transport(_))
        ));

        let mut empty = cfg;
        empty.executable = "  ".to_owned();
        assert!(matches!(
            run_acp_turn(&empty, &mut |_event| {}),
            Err(NaviError::InvalidInput(_))
        ));
    }
}
