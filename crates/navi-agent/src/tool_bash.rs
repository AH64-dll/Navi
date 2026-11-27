//! Built-in `bash` tool: runs a command in a login shell with a wall-clock timeout and bounded output.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Byte cap applied to each stream when the caller does not provide one (64 KiB).
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Interval between checks for child exit and deadline expiry.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Arguments of the built-in `bash` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashToolArgs {
    /// Command passed verbatim to `bash -lc`.
    pub command: String,
    /// Wall-clock limit in milliseconds. `None` means no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Maximum number of trailing bytes retained from each captured stream.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
}

impl Default for BashToolArgs {
    fn default() -> Self {
        Self {
            command: String::new(),
            timeout_ms: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Result of a `bash` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashToolOutput {
    /// Process exit code; signal terminations use the `128 + signal` shell convention.
    pub exit_code: i32,
    /// Retained trailing stdout, decoded as UTF-8.
    pub stdout: String,
    /// Retained trailing stderr, decoded as UTF-8.
    pub stderr: String,
    /// Whether the child was killed because the deadline expired.
    pub timed_out: bool,
    /// Whether either stream was shortened to its trailing `max_output_bytes`.
    pub truncated: bool,
}

/// Runs `bash -lc <command>` inside `workspace_root`, killing the child when the deadline passes.
///
/// Both pipes are drained on dedicated threads, so a full pipe cannot stall the command before
/// it exits. Each stream is tail-truncated independently and always starts on a UTF-8 character
/// boundary; undecodable bytes in the captured tail become U+FFFD rather than invalid UTF-8.
pub fn execute_bash(workspace_root: &Path, args: &BashToolArgs) -> NaviResult<BashToolOutput> {
    if args.command.contains('\0') {
        return Err(NaviError::InvalidInput(
            "bash command must not contain a NUL byte".to_owned(),
        ));
    }
    if !workspace_root.is_dir() {
        return Err(NaviError::NotFound(format!(
            "working directory '{}' does not exist or is not a directory",
            workspace_root.display()
        )));
    }

    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(args.command.as_str())
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| NaviError::Io(format!("failed to spawn bash: {err}")))?;

    let stdout_reader = spawn_reader(child.stdout.take(), "navi-bash-stdout")?;
    let stderr_reader = spawn_reader(child.stderr.take(), "navi-bash-stderr")?;

    let started = Instant::now();
    let deadline = args
        .timeout_ms
        .and_then(|ms| started.checked_add(Duration::from_millis(ms)));
    let (status, timed_out) = wait_with_deadline(&mut child, deadline)?;

    let stdout_bytes = collect_reader(stdout_reader, "stdout")?;
    let stderr_bytes = collect_reader(stderr_reader, "stderr")?;
    let (stdout, stdout_truncated) = tail_truncate(&stdout_bytes, args.max_output_bytes);
    let (stderr, stderr_truncated) = tail_truncate(&stderr_bytes, args.max_output_bytes);

    Ok(BashToolOutput {
        exit_code: exit_code_of(&status),
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// Polls `child` until it exits or `deadline` passes, killing and reaping it on expiry.
fn wait_with_deadline(
    child: &mut Child,
    deadline: Option<Instant>,
) -> NaviResult<(ExitStatus, bool)> {
    loop {
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            // The child may have exited between the previous poll and this check.
            if let Some(status) = child
                .try_wait()
                .map_err(|err| NaviError::Io(format!("failed to poll bash: {err}")))?
            {
                return Ok((status, false));
            }
            let _ = child.kill();
            let status = child
                .wait()
                .map_err(|err| NaviError::Io(format!("failed to reap bash: {err}")))?;
            return Ok((status, true));
        }

        match child.try_wait() {
            Ok(Some(status)) => return Ok((status, false)),
            Ok(None) => {
                let pause = deadline
                    .map(|deadline| {
                        POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))
                    })
                    .unwrap_or(POLL_INTERVAL);
                thread::sleep(pause);
            }
            Err(err) => return Err(NaviError::Io(format!("failed to poll bash: {err}"))),
        }
    }
}

/// Starts a thread that drains one pipe, keeping the reader alive while the child runs.
fn spawn_reader<T: Read + Send + 'static>(
    stream: Option<T>,
    name: &'static str,
) -> NaviResult<JoinHandle<io::Result<Vec<u8>>>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                stream.read_to_end(&mut bytes)?;
            }
            Ok(bytes)
        })
        .map_err(|err| NaviError::Io(format!("failed to start {name} reader: {err}")))
}

/// Joins a reader thread and maps a panic or read failure to a shared error.
fn collect_reader(handle: JoinHandle<io::Result<Vec<u8>>>, label: &str) -> NaviResult<Vec<u8>> {
    match handle.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(err)) => Err(NaviError::Io(format!("failed to read {label}: {err}"))),
        Err(_) => Err(NaviError::Internal(format!(
            "{label} reader thread panicked"
        ))),
    }
}

/// Returns the trailing `limit` bytes as text, skipping any partial leading character.
fn tail_truncate(bytes: &[u8], limit: usize) -> (String, bool) {
    if bytes.len() <= limit {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut start = bytes.len() - limit;
    while start < bytes.len() && is_continuation_byte(bytes[start]) {
        start += 1;
    }
    (String::from_utf8_lossy(&bytes[start..]).into_owned(), true)
}

/// Whether `byte` is a UTF-8 continuation byte, i.e. not the start of a character.
fn is_continuation_byte(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// Converts an exit status to an exit code, using the shell convention for signals.
#[cfg(unix)]
fn exit_code_of(status: &ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(1),
    }
}

/// Converts an exit status to an exit code on platforms without Unix signal access.
#[cfg(not(unix))]
fn exit_code_of(status: &ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Serde fallback used when `max_output_bytes` is absent from serialized arguments.
fn default_max_output_bytes() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("navi-tool-bash-{}-{id}", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).expect("remove stale test directory");
            }
            fs::create_dir_all(&root).expect("create test directory");
            Self { root }
        }

        fn args(
            &self,
            command: impl Into<String>,
            timeout_ms: Option<u64>,
            max_output_bytes: usize,
        ) -> BashToolArgs {
            BashToolArgs {
                command: command.into(),
                timeout_ms,
                max_output_bytes,
            }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn bash_available() -> bool {
        Command::new("bash")
            .arg("-lc")
            .arg("exit 0")
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn runs_command_in_workspace_and_captures_both_streams() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();
        let args = workspace.args(
            "printf 'out\\n'; printf 'err\\n' >&2; printf '%s' \"$PWD\"",
            None,
            4096,
        );

        let output = execute_bash(&workspace.root, &args).expect("run command");

        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.starts_with("out\n"));
        let root = workspace.root.to_string_lossy().into_owned();
        assert!(
            output.stdout.ends_with(&root),
            "unexpected cwd: {}",
            output.stdout
        );
        assert_eq!(output.stderr, "err\n");
        assert!(!output.timed_out);
        assert!(!output.truncated);
    }

    #[test]
    fn reports_nonzero_exit_code_with_output() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("printf before; printf problem >&2; exit 7", None, 1024),
        )
        .expect("run failing command");

        assert_eq!(output.exit_code, 7);
        assert_eq!(output.stdout, "before");
        assert_eq!(output.stderr, "problem");
        assert!(!output.timed_out);
        assert!(!output.truncated);
    }

    #[test]
    fn kills_command_when_timeout_expires() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();
        let started = Instant::now();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("exec sleep 5", Some(40), 1024),
        )
        .expect("run command with timeout");

        assert!(output.timed_out);
        #[cfg(unix)]
        assert_eq!(output.exit_code, 137);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout did not stop the child promptly"
        );
    }

    #[test]
    fn zero_timeout_kills_immediately() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("exec sleep 5", Some(0), 1024),
        )
        .expect("run command with zero timeout");

        assert!(output.timed_out);
    }

    #[test]
    fn command_without_timeout_runs_to_completion() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("printf finished", None, 1024),
        )
        .expect("run command without timeout");

        assert_eq!(output.stdout, "finished");
        assert_eq!(output.exit_code, 0);
        assert!(!output.timed_out);
    }

    #[test]
    fn tail_truncates_stdout_on_utf8_boundary() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();
        let full = "αβγδεζηθ";

        let output = execute_bash(
            &workspace.root,
            &workspace.args(format!("printf '%s' {full}"), None, 5),
        )
        .expect("run unicode command");

        assert!(output.truncated);
        assert!(!output.stdout.contains('\u{fffd}'));
        assert!(output.stdout.len() <= 5);
        assert!(full.ends_with(&output.stdout));
    }

    #[test]
    fn tail_truncates_stderr_independently_of_stdout() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("printf abcdef; printf xyzw >&2", None, 2),
        )
        .expect("run command with output on both streams");

        assert!(output.truncated);
        assert_eq!(output.stdout, "ef");
        assert_eq!(output.stderr, "zw");
    }

    #[test]
    fn leaves_output_untouched_when_it_fits_the_limit() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("printf hi; printf there >&2", None, 64),
        )
        .expect("run small command");

        assert_eq!(output.stdout, "hi");
        assert_eq!(output.stderr, "there");
        assert!(!output.truncated);
    }

    #[test]
    fn empty_output_is_not_marked_truncated() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(&workspace.root, &workspace.args(":", None, 1024))
            .expect("run no-op command");

        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(!output.truncated);
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    fn zero_byte_limit_keeps_no_output() {
        if !bash_available() {
            return;
        }
        let workspace = TempWorkspace::new();

        let output = execute_bash(
            &workspace.root,
            &workspace.args("printf content; printf problem >&2", None, 0),
        )
        .expect("run command with zero output limit");

        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(output.truncated);
    }

    #[test]
    fn rejects_nul_command_and_missing_workspace() {
        let workspace = TempWorkspace::new();

        let error = execute_bash(&workspace.root, &workspace.args("echo\0bad", None, 1024))
            .expect_err("NUL command must be rejected");
        assert!(matches!(error, NaviError::InvalidInput(_)));

        let missing = workspace.root.join("missing-directory");
        let error = execute_bash(&missing, &workspace.args(":", None, 1024))
            .expect_err("missing workspace must be rejected");
        assert!(matches!(error, NaviError::NotFound(_)));
    }

    #[test]
    fn args_and_output_roundtrip_through_json() {
        let args = BashToolArgs {
            command: "echo hi".to_owned(),
            timeout_ms: Some(1500),
            max_output_bytes: 4096,
        };
        let json = serde_json::to_string(&args).expect("serialize args");
        let decoded: BashToolArgs = serde_json::from_str(&json).expect("deserialize args");
        assert_eq!(decoded, args);

        let defaults: BashToolArgs =
            serde_json::from_str(r#"{"command":"echo hi"}"#).expect("deserialize default args");
        assert_eq!(defaults.timeout_ms, None);
        assert_eq!(defaults.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);

        let output = BashToolOutput {
            exit_code: 7,
            stdout: "out".to_owned(),
            stderr: "err".to_owned(),
            timed_out: true,
            truncated: false,
        };
        let json = serde_json::to_string(&output).expect("serialize output");
        let decoded: BashToolOutput = serde_json::from_str(&json).expect("deserialize output");
        assert_eq!(decoded, output);
    }
}
