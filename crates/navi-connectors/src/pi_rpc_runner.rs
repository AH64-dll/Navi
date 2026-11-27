//! Live subprocess runner for Pi's native JSONL RPC protocol.
//!
//! [`run_pi_rpc_turn`] starts the configured executable with piped stdio, sends
//! one `prompt` command built by [`pi_rpc`], and streams parsed [`PiEvent`]
//! values to the caller until `agent_end` arrives. Frames use the same limits
//! as the Python worker: a per-record byte cap, a total byte budget, and a
//! nesting guard capped by [`NAVI_PI_DEPTH`]. One deadline covers prompt
//! delivery, event streaming, and child exit; on failure the child is killed
//! and reaped before the error is returned.

use crate::pi_rpc::{self, DEFAULT_MAX_BYTES, NAVI_PI_DEPTH, PiEvent};
use navi_types::{ContentBlock, Message, NaviError, NaviResult, Role};
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const PROMPT_ID: &str = "prompt-1";
const MAX_TIMEOUT_MS: u64 = 1_800_000;
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const OVERSIZE_FRAME: &str = "Pi record exceeded its limit or was truncated";
const BUDGET_EXHAUSTED: &str = "Pi total record budget exhausted";
const CLOSED_EARLY: &str = "Pi connection closed before the turn settled";
const TURN_TIMED_OUT: &str = "Pi turn timed out";
const ANSWER_BUDGET: &str = "Pi answer budget exhausted";
const PROMPT_REJECTED: &str = "Pi rejected the prompt";

/// Parameters for one bounded Pi RPC subprocess turn.
///
/// `cwd` selects the child's working directory; `None` inherits the caller's.
/// `max_depth` is the frame nesting limit and is never raised above
/// [`NAVI_PI_DEPTH`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiRpcRunConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub prompt: String,
    pub timeout_ms: u64,
    pub max_depth: usize,
}

impl Default for PiRpcRunConfig {
    fn default() -> Self {
        Self {
            executable: "pi".to_owned(),
            args: Vec::new(),
            cwd: None,
            prompt: String::new(),
            timeout_ms: 180_000,
            max_depth: NAVI_PI_DEPTH,
        }
    }
}

/// The answer, thinking trace, normalized events, and exit status of a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct PiRpcTurnOutcome {
    pub reply_text: String,
    pub thinking_text: String,
    pub events: Vec<PiEvent>,
    pub exit_code: i32,
}

/// Runs one Pi RPC turn in a child process and reports every parsed event.
///
/// The callback runs on the caller's thread in frame order. An explicit
/// `response` failure, a malformed frame, an early EOF, or an expired deadline
/// produces an error; the child is terminated and reaped in those cases.
pub fn run_pi_rpc_turn(
    cfg: &PiRpcRunConfig,
    on_event: &mut dyn FnMut(&PiEvent),
) -> NaviResult<PiRpcTurnOutcome> {
    validate_config(cfg)?;
    let command = pi_rpc::build_prompt(PROMPT_ID, &cfg.prompt)?;
    let prompt_bytes = pi_rpc::encode_line(&command)?;

    let mut spawn = Command::new(&cfg.executable);
    spawn
        .args(cfg.args.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cfg.cwd.as_deref() {
        spawn.current_dir(dir);
    }

    let deadline = Instant::now()
        .checked_add(Duration::from_millis(cfg.timeout_ms))
        .ok_or_else(|| NaviError::Internal("Pi turn deadline overflowed".into()))?;
    let executable = &cfg.executable;
    let mut child = spawn.spawn().map_err(|err| {
        NaviError::Io(format!(
            "failed to start Pi executable '{executable}': {err}"
        ))
    })?;

    match drive_turn(&mut child, cfg, &prompt_bytes, deadline, on_event) {
        Ok((state, status)) => Ok(PiRpcTurnOutcome {
            reply_text: state.reply_text,
            thinking_text: state.thinking_text,
            events: state.events,
            exit_code: status.code().unwrap_or(-1),
        }),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(err)
        }
    }
}

fn validate_config(cfg: &PiRpcRunConfig) -> NaviResult<()> {
    if cfg.executable.is_empty() || cfg.executable.contains('\0') {
        return Err(NaviError::InvalidInput("Invalid Pi executable".into()));
    }
    if cfg.args.iter().any(|arg| arg.contains('\0')) {
        return Err(NaviError::InvalidInput("Invalid Pi argument".into()));
    }
    if let Some(dir) = cfg.cwd.as_deref()
        && (dir.as_os_str().is_empty() || dir.as_os_str().as_encoded_bytes().contains(&0))
    {
        return Err(NaviError::InvalidInput(
            "Invalid Pi working directory".into(),
        ));
    }
    if cfg.timeout_ms == 0 || cfg.timeout_ms > MAX_TIMEOUT_MS {
        return Err(NaviError::InvalidInput(
            "Invalid Pi turn limits: timeout_ms must be in 1..=1800000".into(),
        ));
    }
    if cfg.max_depth == 0 || cfg.max_depth > NAVI_PI_DEPTH {
        return Err(NaviError::InvalidInput(format!(
            "Invalid Pi depth limit: max_depth must be in 1..={NAVI_PI_DEPTH}"
        )));
    }
    Ok(())
}

/// Everything collected while frames stream, before the exit status is known.
#[derive(Debug, Default)]
struct TurnState {
    reply_text: String,
    thinking_text: String,
    events: Vec<PiEvent>,
}

/// A frame, a reader-side failure, or a closed stdout pipe.
enum ReaderMessage {
    Frame(Value),
    Error(NaviError),
    Eof,
}

fn drive_turn(
    child: &mut Child,
    cfg: &PiRpcRunConfig,
    prompt_bytes: &[u8],
    deadline: Instant,
    on_event: &mut dyn FnMut(&PiEvent),
) -> NaviResult<(TurnState, ExitStatus)> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| NaviError::Internal("Pi stdin pipe is missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| NaviError::Internal("Pi stdout pipe is missing".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| NaviError::Internal("Pi stderr pipe is missing".into()))?;

    // A child that writes a lot of stderr must not block on a full pipe.
    let _stderr_thread = thread::spawn(move || {
        let mut sink = io::sink();
        let _ = io::copy(&mut stderr, &mut sink);
    });

    let (write_tx, write_rx) = mpsc::sync_channel(1);
    let writer = thread::spawn({
        let prompt_bytes = prompt_bytes.to_vec();
        move || {
            let result = stdin
                .write_all(&prompt_bytes)
                .and_then(|()| stdin.flush())
                .map_err(NaviError::from);
            let _ = write_tx.send(result);
            // Dropping stdin here gives Pi EOF after the single command.
        }
    });

    let depth = cfg.max_depth.min(NAVI_PI_DEPTH);
    let (frame_tx, frame_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut frames = FrameReader::new(stdout, depth);
        loop {
            let message = match frames.next_frame() {
                Ok(Some(value)) => ReaderMessage::Frame(value),
                Ok(None) => ReaderMessage::Eof,
                Err(err) => ReaderMessage::Error(err),
            };
            let terminal = matches!(&message, ReaderMessage::Eof | ReaderMessage::Error(_));
            if frame_tx.send(message).is_err() || terminal {
                break;
            }
        }
    });

    match write_rx.recv_timeout(remaining_until(deadline)?) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err),
        Err(RecvTimeoutError::Timeout) => return Err(NaviError::Timeout(TURN_TIMED_OUT.into())),
        Err(RecvTimeoutError::Disconnected) => {
            return Err(NaviError::Internal("Pi writer thread stopped".into()));
        }
    }
    writer
        .join()
        .map_err(|_| NaviError::Internal("Pi writer thread panicked".into()))?;

    let mut state = TurnState::default();
    loop {
        match frame_rx.recv_timeout(remaining_until(deadline)?) {
            Ok(ReaderMessage::Frame(frame)) => {
                if pi_rpc::response_for(&frame, PROMPT_ID)? == Some(false) {
                    return Err(NaviError::Protocol(PROMPT_REJECTED.into()));
                }
                if let Some(event) = pi_rpc::parse_event(&frame)? {
                    let settled = matches!(&event, PiEvent::AgentEnd { .. });
                    record_event(event, &mut state, on_event)?;
                    if settled {
                        break;
                    }
                }
            }
            Ok(ReaderMessage::Error(err)) => return Err(err),
            Ok(ReaderMessage::Eof) => return Err(NaviError::Protocol(CLOSED_EARLY.into())),
            Err(RecvTimeoutError::Timeout) => {
                return Err(NaviError::Timeout(TURN_TIMED_OUT.into()));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(NaviError::Internal("Pi reader thread stopped".into()));
            }
        }
    }

    let status = wait_for_exit(child, deadline)?;
    if drain_to_eof(&frame_rx, deadline) {
        let _ = reader.join();
    }
    Ok((state, status))
}

fn remaining_until(deadline: Instant) -> NaviResult<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(NaviError::Timeout(TURN_TIMED_OUT.into()))
    } else {
        Ok(remaining)
    }
}

fn wait_for_exit(child: &mut Child, deadline: Instant) -> NaviResult<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(NaviError::Timeout(
                "Pi did not exit after the turn settled".into(),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Keeps draining stdout after `agent_end` so the child cannot block writing.
/// Returns whether the reader announced a clean EOF (safe to join).
fn drain_to_eof(frames: &Receiver<ReaderMessage>, deadline: Instant) -> bool {
    loop {
        let Ok(remaining) = remaining_until(deadline) else {
            return false;
        };
        match frames.recv_timeout(remaining) {
            Ok(ReaderMessage::Eof) => return true,
            Ok(ReaderMessage::Frame(_)) => continue,
            // An error is terminal for the reader: no EOF will follow it.
            Ok(ReaderMessage::Error(_)) => return false,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn record_event(
    event: PiEvent,
    state: &mut TurnState,
    on_event: &mut dyn FnMut(&PiEvent),
) -> NaviResult<()> {
    match &event {
        PiEvent::TextDelta { delta, .. } => append_bounded(&mut state.reply_text, delta)?,
        PiEvent::ThinkingDelta { delta, .. } => append_bounded(&mut state.thinking_text, delta)?,
        PiEvent::AgentEnd { messages } => {
            let (text, thinking) = message_text(messages)?;
            // The settled message is authoritative; streaming deltas are the
            // fallback when the child omits the final message content.
            if !text.is_empty() {
                state.reply_text = text;
            }
            if !thinking.is_empty() {
                state.thinking_text = thinking;
            }
        }
        _ => {}
    }
    on_event(&event);
    state.events.push(event);
    Ok(())
}

fn message_text(messages: &[Message]) -> NaviResult<(String, String)> {
    let mut text = String::new();
    let mut thinking = String::new();
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for block in &message.content {
            match block {
                ContentBlock::Text { text: value } => append_bounded(&mut text, value)?,
                ContentBlock::Thinking {
                    thinking: value, ..
                } => append_bounded(&mut thinking, value)?,
                _ => {}
            }
        }
    }
    Ok((text, thinking))
}

fn append_bounded(target: &mut String, delta: &str) -> NaviResult<()> {
    if target.len().saturating_add(delta.len()) > DEFAULT_MAX_BYTES {
        return Err(NaviError::Protocol(ANSWER_BUDGET.into()));
    }
    target.push_str(delta);
    Ok(())
}

/// LF-only frame reader with per-record and total byte budgets.
///
/// Raw bytes are checked by [`pi_rpc::parse_frame_with_depth`] before any event
/// parsing, so hostile nesting never reaches recursive JSON handling.
struct FrameReader<R> {
    input: BufReader<R>,
    max_depth: usize,
    max_bytes: usize,
    total_limit: usize,
    received: usize,
}

impl<R: Read> FrameReader<R> {
    fn new(input: R, max_depth: usize) -> Self {
        Self::with_limits(
            input,
            max_depth,
            DEFAULT_MAX_BYTES,
            DEFAULT_MAX_BYTES.saturating_mul(8),
        )
    }

    fn with_limits(input: R, max_depth: usize, max_bytes: usize, total_limit: usize) -> Self {
        Self {
            input: BufReader::new(input),
            max_depth,
            max_bytes,
            total_limit,
            received: 0,
        }
    }

    fn next_frame(&mut self) -> NaviResult<Option<Value>> {
        let allowance = self.total_limit.saturating_sub(self.received);
        if allowance == 0 || self.max_bytes == 0 {
            return Err(NaviError::Protocol(BUDGET_EXHAUSTED.into()));
        }
        let per_record = self.max_bytes.min(allowance);
        let limit = u64::try_from(per_record).unwrap_or(u64::MAX);
        let mut line = Vec::new();
        let read = (&mut self.input).take(limit).read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() > per_record || !line.ends_with(b"\n") {
            return Err(NaviError::Protocol(OVERSIZE_FRAME.into()));
        }
        self.received = self.received.saturating_add(line.len());
        pi_rpc::parse_frame_with_depth(&line, self.max_depth).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    const AGENT_END: &str = r#"{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"hello"}],"stopReason":"stop"}]}"#;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let process_id = std::process::id();
        let path = std::env::temp_dir().join(format!("navi-pi-rpc-{label}-{process_id}-{id}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn config(script: &str, prompt: &str, timeout_ms: u64) -> PiRpcRunConfig {
        PiRpcRunConfig {
            executable: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            cwd: None,
            prompt: prompt.to_owned(),
            timeout_ms,
            max_depth: NAVI_PI_DEPTH,
        }
    }

    /// Builds a shell responder that consumes the prompt line, then prints
    /// each frame as one LF-terminated JSON record.
    fn script_with_frames(frames: &[&str]) -> String {
        let mut script = String::from("IFS= read -r line\nprintf '%s\\n'");
        for frame in frames {
            script.push_str(" '");
            script.push_str(frame);
            script.push('\'');
        }
        script.push('\n');
        script
    }

    #[test]
    fn streams_events_and_collects_reply_and_thinking() {
        let script = script_with_frames(&[
            r#"{"type":"response","id":"prompt-1","success":true}"#,
            r#"{"type":"agent_start"}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hel","contentIndex":0}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"hmm"}}"#,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"lo"}}"#,
            AGENT_END,
        ]);
        let cfg = config(&script, "say hello", 3_000);
        let mut seen: Vec<PiEvent> = Vec::new();
        let outcome = run_pi_rpc_turn(&cfg, &mut |event| seen.push(event.clone()))
            .expect("mock Pi turn should succeed");

        assert_eq!(outcome.reply_text, "hello");
        assert_eq!(outcome.thinking_text, "hmm");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.events.len(), 4);
        assert_eq!(seen, outcome.events);
        assert!(matches!(outcome.events[0], PiEvent::TextDelta { delta: ref d, .. } if d == "hel"));
        assert!(
            matches!(outcome.events[1], PiEvent::ThinkingDelta { delta: ref d, .. } if d == "hmm")
        );
        assert!(matches!(outcome.events[2], PiEvent::TextDelta { delta: ref d, .. } if d == "lo"));
        assert!(matches!(outcome.events[3], PiEvent::AgentEnd { .. }));
    }

    #[test]
    fn honors_cwd_and_delivers_the_prompt_command() {
        let dir = temp_dir("cwd");
        let trace = dir.join("trace.txt");
        let script = format!(
            "IFS= read -r line\nprintf '%s\\n' \"$line\" > \"$1\"\npwd >> \"$1\"\nprintf '%s\\n' '{AGENT_END}'\n"
        );
        let cfg = PiRpcRunConfig {
            executable: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                script,
                "sh".to_owned(),
                trace.to_string_lossy().into_owned(),
            ],
            cwd: Some(dir.clone()),
            prompt: "deliver me".to_owned(),
            timeout_ms: 3_000,
            max_depth: NAVI_PI_DEPTH,
        };

        let outcome = run_pi_rpc_turn(&cfg, &mut |_| {}).expect("turn with cwd");
        assert_eq!(outcome.reply_text, "hello");

        let written = fs::read_to_string(&trace).expect("read trace");
        let command: Value = serde_json::from_str(written.lines().next().expect("prompt line"))
            .expect("prompt is JSONL");
        assert_eq!(command["id"], json!(PROMPT_ID));
        assert_eq!(command["type"], json!("prompt"));
        assert_eq!(command["message"], json!("deliver me"));
        let expected = fs::canonicalize(&dir).expect("canonical cwd");
        assert!(
            written
                .lines()
                .skip(1)
                .any(|line| Path::new(line) == expected.as_path())
        );
        fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn times_out_and_reaps_a_child_that_never_settles() {
        let cfg = config("IFS= read -r line\nexec sleep 5\n", "wait", 120);
        let started = Instant::now();
        let err = run_pi_rpc_turn(&cfg, &mut |_| {}).expect_err("must time out");
        assert!(matches!(err, NaviError::Timeout(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn rejects_invalid_config_before_spawning() {
        let script = script_with_frames(&[AGENT_END]);
        let mut cfg = config(&script, "hello", 1_000);

        cfg.prompt = "  ".to_owned();
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));

        cfg = config(&script, "hello", 1_000);
        cfg.executable.clear();
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));

        cfg = config(&script, "hello", 0);
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));

        cfg = config(&script, "hello", MAX_TIMEOUT_MS + 1);
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));

        cfg = config(&script, "hello", 1_000);
        cfg.max_depth = 0;
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));

        cfg = config(&script, "hello", 1_000);
        cfg.args = vec!["bad\0arg".to_owned()];
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));

        cfg = config(&script, "hello", 1_000);
        cfg.cwd = Some(PathBuf::from("\0"));
        assert!(matches!(
            run_pi_rpc_turn(&cfg, &mut |_| {}),
            Err(NaviError::InvalidInput(_))
        ));
    }

    #[test]
    fn errors_when_stdout_closes_before_agent_end() {
        let script = script_with_frames(&[
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"partial"}}"#,
        ]);
        let cfg = config(&script, "hello", 2_000);
        let err = run_pi_rpc_turn(&cfg, &mut |_| {}).expect_err("no agent_end");
        assert!(matches!(err, NaviError::Protocol(m) if m.contains("closed")));
    }

    #[test]
    fn rejects_an_explicit_prompt_failure() {
        let script = script_with_frames(&[
            r#"{"type":"response","id":"prompt-1","success":false}"#,
            AGENT_END,
        ]);
        let cfg = config(&script, "hello", 2_000);
        let err = run_pi_rpc_turn(&cfg, &mut |_| {}).expect_err("rejected prompt");
        assert!(matches!(err, NaviError::Protocol(m) if m.contains("rejected")));
    }

    #[test]
    fn frame_reader_enforces_byte_budgets_depth_and_object_shape() {
        // Three 8-byte records; a 16-byte total budget admits the first two
        // and makes the third call fail with a budget error.
        let data = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        let mut reader = FrameReader::with_limits(&data[..], 8, 8, 16);
        assert_eq!(
            reader.next_frame().expect("first frame"),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            reader.next_frame().expect("second frame"),
            Some(json!({"b": 2}))
        );
        assert!(matches!(reader.next_frame(), Err(NaviError::Protocol(m)) if m.contains("budget")));

        let mut oversize = FrameReader::with_limits(&b"{\"a\":1}\n"[..], 8, 4, 64);
        assert!(
            matches!(oversize.next_frame(), Err(NaviError::Protocol(m)) if m.contains("limit"))
        );
        let mut truncated = FrameReader::with_limits(&b"{\"a\":1}"[..], 8, 64, 64);
        assert!(truncated.next_frame().is_err());

        let deep = b"{\"x\":[[[1]]]}\n";
        let mut guarded = FrameReader::with_limits(&deep[..], 3, 64, 64);
        assert!(guarded.next_frame().is_err());
        let mut allowed = FrameReader::with_limits(&deep[..], 4, 64, 64);
        assert!(allowed.next_frame().expect("depth allowed").is_some());

        let mut non_object = FrameReader::with_limits(&b"[1,2]\n"[..], 8, 64, 64);
        assert!(non_object.next_frame().is_err());
    }

    #[test]
    fn subprocess_depth_limit_is_capped_and_configurable() {
        let opens = "[".repeat(4);
        let closes = "]".repeat(4);
        let deep = format!("{{\"type\":\"pad_event\",\"pad\":{opens}{closes}}}");
        let script = script_with_frames(&[&deep, AGENT_END]);

        let mut cfg = config(&script, "hello", 2_000);
        cfg.max_depth = 3;
        let err = run_pi_rpc_turn(&cfg, &mut |_| {}).expect_err("depth guard");
        assert!(matches!(err, NaviError::Protocol(m) if m.contains("invalid JSON")));

        let cfg = config(&script, "hello", 2_000);
        let outcome = run_pi_rpc_turn(&cfg, &mut |_| {}).expect("depth within the guard");
        assert_eq!(outcome.reply_text, "hello");
        assert_eq!(outcome.events.len(), 1);
    }

    #[test]
    fn forwards_tool_events_and_reports_nonzero_exit_code() {
        let frames = script_with_frames(&[
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"read","args":{"path":"a"}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"read","result":{"ok":true},"isError":false}"#,
            AGENT_END,
        ]);
        let script = format!("{frames}\nexit 3\n");
        let cfg = config(&script, "hello", 2_000);
        let outcome = run_pi_rpc_turn(&cfg, &mut |_| {}).expect("turn with tools");

        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.reply_text, "hello");
        assert_eq!(outcome.events.len(), 3);
        assert!(
            matches!(outcome.events[0], PiEvent::ToolExecutionStart { ref tool_call_id, .. } if tool_call_id == "c1")
        );
        assert!(matches!(
            outcome.events[1],
            PiEvent::ToolExecutionEnd {
                is_error: false,
                ..
            }
        ));
        assert!(matches!(outcome.events[2], PiEvent::AgentEnd { .. }));
    }
}
