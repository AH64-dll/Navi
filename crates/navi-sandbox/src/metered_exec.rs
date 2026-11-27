//! Dual-metered command execution: a wall-clock deadline plus a strict,
//! combined stdout/stderr byte budget.
//!
//! The child gets a cleared environment with only allowlisted variables,
//! denied command names are rejected before spawning, both output pipes are
//! drained continuously (so a chatty child cannot block on a full pipe), and
//! a child that outlives its deadline is killed.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Limits applied to every call of [`run_sandboxed_command`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxExecPolicy {
    /// Working directory for the child; it must already exist.
    pub cwd: PathBuf,
    /// Wall-clock deadline in milliseconds; must be greater than zero.
    pub timeout_ms: u64,
    /// Combined byte budget for the child's stdout and stderr.
    pub max_output_bytes: usize,
    /// Environment variable names copied from this process into the child.
    pub env_allowlist: Vec<String>,
    /// Command names rejected before spawning, matched on basename ignoring case.
    pub denied_commands: Vec<String>,
}

impl Default for SandboxExecPolicy {
    /// A conservative general-purpose policy: five seconds, one MiB of output,
    /// `PATH` inherited so bare program names can be resolved, no denials.
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            timeout_ms: 5_000,
            max_output_bytes: 1024 * 1024,
            env_allowlist: vec!["PATH".to_string()],
            denied_commands: Vec::new(),
        }
    }
}

/// Outcome of one sandboxed execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxExecResult {
    /// Process exit status, or `-1` when the child was killed or signalled.
    pub exit_code: i32,
    /// Captured stdout bytes, never longer than the policy budget.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes, never longer than the policy budget.
    pub stderr: Vec<u8>,
    /// Wall-clock time from spawn to child exit (or kill), in milliseconds.
    pub duration_ms: u64,
    /// True when the deadline expired and the child was killed.
    pub timed_out: bool,
    /// True when the child wrote more bytes than the shared budget allowed.
    pub output_truncated: bool,
}

/// Runs `program` with `args` under the meters and restrictions in `policy`.
///
/// Returns an error for an invalid policy, a denied command, a spawn failure,
/// or a failure to reap the child. A deadline expiry is *not* an error: it is
/// reported through `SandboxExecResult::timed_out`.
pub fn run_sandboxed_command(
    program: &str,
    args: &[&str],
    policy: &SandboxExecPolicy,
) -> NaviResult<SandboxExecResult> {
    validate_policy(policy)?;
    if program.trim().is_empty() {
        return Err(NaviError::InvalidInput(
            "program must not be empty".to_string(),
        ));
    }

    let denied = denied_names(&policy.denied_commands);
    if is_denied(program, args, &denied) {
        return Err(NaviError::PermissionDenied(format!(
            "command '{program}' is denied by policy"
        )));
    }

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&policy.cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in &policy.env_allowlist {
        if let Some(value) = std::env::var_os(name.as_str()) {
            command.env(name.as_str(), value);
        }
    }

    let start = Instant::now();
    let mut child = command.spawn()?;

    // One budget shared by both streams. Readers keep draining after the
    // budget is exhausted so the child never blocks on a full pipe.
    let capture = Arc::new(Mutex::new(OutputCapture::new(policy.max_output_bytes)));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| NaviError::Internal("child stdout pipe is missing".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| NaviError::Internal("child stderr pipe is missing".to_string()))?;

    let stdout_capture = Arc::clone(&capture);
    let stdout_thread =
        thread::spawn(move || drain_stream(stdout, stdout_capture, OutputStream::Stdout));
    let stderr_capture = Arc::clone(&capture);
    let stderr_thread =
        thread::spawn(move || drain_stream(stderr, stderr_capture, OutputStream::Stderr));

    let timeout = Duration::from_millis(policy.timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NaviError::Io(err.to_string()));
            }
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            // The child may already have exited between the poll and the kill.
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(2));
    };
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    if !timed_out {
        // On timeout a descendant may still hold an inherited pipe open, so
        // joining here could outlive the deadline we just enforced.
        stdout_thread
            .join()
            .map_err(|_| NaviError::Internal("stdout reader thread panicked".to_string()))?;
        stderr_thread
            .join()
            .map_err(|_| NaviError::Internal("stderr reader thread panicked".to_string()))?;
    }

    let mut output = capture.lock().unwrap_or_else(PoisonError::into_inner);
    Ok(SandboxExecResult {
        exit_code: status.code().unwrap_or(-1),
        stdout: std::mem::take(&mut output.stdout),
        stderr: std::mem::take(&mut output.stderr),
        duration_ms,
        timed_out,
        output_truncated: output.truncated,
    })
}

/// Rejects policies that cannot be enforced as written.
fn validate_policy(policy: &SandboxExecPolicy) -> NaviResult<()> {
    if policy.timeout_ms == 0 {
        return Err(NaviError::InvalidInput(
            "timeout_ms must be greater than zero".to_string(),
        ));
    }
    if !policy.cwd.is_dir() {
        return Err(NaviError::NotFound(format!(
            "working directory '{}' is not a directory",
            policy.cwd.display()
        )));
    }
    for name in &policy.env_allowlist {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(NaviError::InvalidInput(format!(
                "invalid environment variable name '{name}'"
            )));
        }
    }
    Ok(())
}

/// Which sink a reader thread writes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

/// Bounded, shared capture state written by the two reader threads.
#[derive(Debug)]
struct OutputCapture {
    remaining: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl OutputCapture {
    /// Creates a capture with the given combined byte budget.
    fn new(budget: usize) -> Self {
        Self {
            remaining: budget,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        }
    }

    /// Keeps as much of `chunk` as the budget allows, discarding the rest.
    fn accept(&mut self, chunk: &[u8], stream: OutputStream) {
        if chunk.len() > self.remaining {
            self.truncated = true;
        }
        let kept = chunk.len().min(self.remaining);
        if kept == 0 {
            return;
        }
        self.remaining -= kept;
        match stream {
            OutputStream::Stdout => self.stdout.extend_from_slice(&chunk[..kept]),
            OutputStream::Stderr => self.stderr.extend_from_slice(&chunk[..kept]),
        }
    }
}

/// Reads one pipe to EOF, forwarding bytes into the shared budget.
fn drain_stream<R: Read>(mut reader: R, capture: Arc<Mutex<OutputCapture>>, stream: OutputStream) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let mut guard = capture.lock().unwrap_or_else(PoisonError::into_inner);
                guard.accept(&buffer[..count], stream);
            }
        }
    }
}

/// Normalizes policy entries into lowercase command basenames.
fn denied_names(entries: &[String]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|name| {
            let normalized = command_basename(name.trim()).to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect()
}

/// Returns the final path component of a token.
fn command_basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or_default()
}

/// Characters that separate command tokens inside an argument string.
fn is_separator(ch: char) -> bool {
    ch.is_ascii_whitespace()
        || matches!(
            ch,
            ';' | '|' | '&' | '(' | ')' | '<' | '>' | '`' | '$' | '\'' | '"'
        )
}

/// Checks the program and every shell-like token in the arguments.
fn is_denied(program: &str, args: &[&str], denied: &BTreeSet<String>) -> bool {
    if denied.contains(&command_basename(program).to_ascii_lowercase()) {
        return true;
    }
    args.iter().any(|arg| {
        arg.split(is_separator)
            .filter(|token| !token.is_empty())
            .any(|token| denied.contains(&command_basename(token).to_ascii_lowercase()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with(dir: &str, timeout_ms: u64, budget: usize) -> SandboxExecPolicy {
        SandboxExecPolicy {
            cwd: PathBuf::from(dir),
            timeout_ms,
            max_output_bytes: budget,
            ..SandboxExecPolicy::default()
        }
    }

    #[test]
    fn captures_stdout_and_success_status() {
        let policy = policy_with("/tmp", 2_000, 4_096);
        let result = run_sandboxed_command("/bin/sh", &["-c", "printf hello"], &policy)
            .expect("command should run");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, b"hello".to_vec());
        assert!(result.stderr.is_empty());
        assert!(!result.timed_out);
        assert!(!result.output_truncated);
    }

    #[test]
    fn reports_nonzero_exit_code_and_stderr() {
        let policy = policy_with("/tmp", 2_000, 4_096);
        let result = run_sandboxed_command("/bin/sh", &["-c", "printf boom >&2; exit 7"], &policy)
            .expect("command should run");
        assert_eq!(result.exit_code, 7);
        assert_eq!(result.stderr, b"boom".to_vec());
        assert!(result.stdout.is_empty());
        assert!(!result.timed_out);
    }

    #[test]
    fn denies_program_by_basename() {
        let mut policy = policy_with("/tmp", 2_000, 4_096);
        policy.denied_commands = vec!["rm".to_string()];

        let err = run_sandboxed_command("rm", &["-rf", "/tmp/unused"], &policy)
            .expect_err("bare name should be denied");
        assert!(matches!(err, NaviError::PermissionDenied(_)));

        let err = run_sandboxed_command("/usr/bin/rm", &[], &policy)
            .expect_err("path should be denied by basename");
        assert!(matches!(err, NaviError::PermissionDenied(_)));

        let err =
            run_sandboxed_command("RM", &[], &policy).expect_err("matching should ignore case");
        assert!(matches!(err, NaviError::PermissionDenied(_)));
    }

    #[test]
    fn denies_command_embedded_in_shell_arguments() {
        let mut policy = policy_with("/tmp", 2_000, 4_096);
        policy.denied_commands = vec!["curl".to_string()];
        let err = run_sandboxed_command("/bin/sh", &["-c", "curl https://example.com"], &policy)
            .expect_err("shell argument token should be denied");
        assert!(matches!(err, NaviError::PermissionDenied(_)));
    }

    #[test]
    fn environment_is_cleared_except_allowlisted_names() {
        let mut policy = policy_with("/tmp", 2_000, 4_096);
        policy.env_allowlist = Vec::new();
        let result = run_sandboxed_command(
            "/bin/sh",
            &["-c", "printf '%s' \"${HOME:-unset}\""],
            &policy,
        )
        .expect("command should run");
        assert_eq!(result.stdout, b"unset".to_vec());

        let parent_path = std::env::var("PATH").expect("PATH is set and UTF-8");
        policy.env_allowlist = vec!["PATH".to_string()];
        let result = run_sandboxed_command("/bin/sh", &["-c", "printf '%s' \"$PATH\""], &policy)
            .expect("command should run");
        assert_eq!(result.stdout, parent_path.into_bytes());
    }

    #[test]
    fn enforces_strict_output_budget() {
        let result = run_sandboxed_command(
            "/bin/sh",
            &["-c", "printf 0123456789"],
            &policy_with("/tmp", 2_000, 4),
        )
        .expect("command should run");
        assert_eq!(result.stdout, b"0123".to_vec());
        assert!(result.output_truncated);

        let result = run_sandboxed_command(
            "/bin/sh",
            &["-c", "printf 0123456789"],
            &policy_with("/tmp", 2_000, 10),
        )
        .expect("command should run");
        assert_eq!(result.stdout, b"0123456789".to_vec());
        assert!(!result.output_truncated);

        let result = run_sandboxed_command(
            "/bin/sh",
            &["-c", "printf x"],
            &policy_with("/tmp", 2_000, 0),
        )
        .expect("command should run");
        assert!(result.stdout.is_empty());
        assert!(result.output_truncated);
    }

    #[test]
    fn budget_is_shared_between_stdout_and_stderr() {
        let result = run_sandboxed_command(
            "/bin/sh",
            &["-c", "printf abc; printf def >&2"],
            &policy_with("/tmp", 2_000, 6),
        )
        .expect("command should run");
        assert_eq!(result.stdout.len() + result.stderr.len(), 6);
        assert!(!result.output_truncated);
    }

    #[test]
    fn timeout_kills_child() {
        let started = Instant::now();
        let result = run_sandboxed_command(
            "/bin/sh",
            &["-c", "while :; do :; done"],
            &policy_with("/tmp", 150, 1_024),
        )
        .expect("command should run");
        assert!(result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(result.duration_ms >= 100);
        assert_eq!(result.exit_code, -1);
    }

    #[test]
    fn rejects_invalid_policy_and_program() {
        let err = run_sandboxed_command("/bin/sh", &[], &policy_with("/tmp", 0, 100))
            .expect_err("zero timeout should be rejected");
        assert!(matches!(err, NaviError::InvalidInput(_)));

        let mut policy = policy_with("/tmp", 1_000, 100);
        policy.env_allowlist = vec!["BAD=NAME".to_string()];
        let err = run_sandboxed_command("/bin/sh", &[], &policy)
            .expect_err("malformed env name should be rejected");
        assert!(matches!(err, NaviError::InvalidInput(_)));

        let err = run_sandboxed_command(
            "/bin/sh",
            &[],
            &policy_with("/no/such/working/directory", 1_000, 100),
        )
        .expect_err("missing cwd should be rejected");
        assert!(matches!(err, NaviError::NotFound(_)));

        let err = run_sandboxed_command("", &[], &policy_with("/tmp", 1_000, 100))
            .expect_err("empty program should be rejected");
        assert!(matches!(err, NaviError::InvalidInput(_)));
    }

    #[test]
    fn policy_and_result_roundtrip_through_json() {
        let policy = policy_with("/tmp", 1_234, 512);
        let encoded = serde_json::to_string(&policy).expect("serialize policy");
        let decoded: SandboxExecPolicy =
            serde_json::from_str(&encoded).expect("deserialize policy");
        assert_eq!(decoded, policy);

        let result = SandboxExecResult {
            exit_code: 0,
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
            duration_ms: 12,
            timed_out: false,
            output_truncated: false,
        };
        let encoded = serde_json::to_string(&result).expect("serialize result");
        let decoded: SandboxExecResult =
            serde_json::from_str(&encoded).expect("deserialize result");
        assert_eq!(decoded, result);
    }

    #[test]
    fn spawn_failure_is_reported_as_io_error() {
        let err = run_sandboxed_command(
            "/nonexistent/navi-metered-exec-binary",
            &[],
            &policy_with("/tmp", 1_000, 100),
        )
        .expect_err("a missing program must fail to spawn");
        assert!(matches!(err, NaviError::Io(_)));
    }
}
