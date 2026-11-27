//! Unix domain socket JSON-line RPC client ported from `apps/navi/workers/ipc.py`.
//!
//! The wire format is one compact JSON object per line. A request carries `op`
//! and `params`; a successful response carries `ok: true` and `result`, while a
//! failed one carries `ok: false` and `error`.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use navi_types::{NaviError, NaviResult};

/// Upper bound for a response line, matching Python's `readline(4 * 1024 * 1024)`.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Default params object, matching Python's `params or {}`.
fn empty_params() -> Value {
    Value::Object(Map::new())
}

/// One RPC invocation sent to the station daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    /// Operation name, for example `status`.
    pub op: String,
    /// Arguments for the operation; empty object when omitted in Python.
    #[serde(default = "empty_params")]
    pub params: Value,
}

impl IpcRequest {
    /// Creates a request with explicit params.
    pub fn new(op: impl Into<String>, params: Value) -> Self {
        Self {
            op: op.into(),
            params,
        }
    }

    /// Creates a request with empty params, matching Python's `params or {}`.
    pub fn without_params(op: impl Into<String>) -> Self {
        Self::new(op, empty_params())
    }

    /// Encodes this request as a single NDJSON frame ready to write to a socket.
    pub fn to_frame(&self) -> NaviResult<String> {
        encode_ndjson_line(self)
    }
}

/// Response returned by the station daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    /// `true` when the call succeeded, `false` when the daemon reported an error.
    pub ok: bool,
    /// Error text present when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Result value present when `ok` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

impl IpcResponse {
    /// Builds a successful response.
    pub fn success(result: Value) -> Self {
        Self {
            ok: true,
            error: None,
            result: Some(result),
        }
    }

    /// Builds a failed response carrying the daemon's message.
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            result: None,
        }
    }

    /// Turns the wire response into a result, mirroring Python's error branch.
    pub fn into_result(self) -> NaviResult<Value> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            let message = self.error.unwrap_or_default();
            Err(NaviError::Protocol(format!("IPC call failed: {message}")))
        }
    }
}

/// Encodes `value` as one compact JSON line terminated by a single `\n`.
pub fn encode_ndjson_line<T: Serialize>(value: &T) -> NaviResult<String> {
    let json = serde_json::to_string(value)
        .map_err(|e| NaviError::Protocol(format!("cannot encode IPC frame: {e}")))?;
    Ok(format!("{json}\n"))
}

/// Decodes one JSON line into `T`, tolerating surrounding whitespace.
pub fn decode_ndjson_line<T: DeserializeOwned>(line: &str) -> NaviResult<T> {
    let frame = line.trim();
    if frame.is_empty() {
        return Err(NaviError::Protocol("empty IPC frame".into()));
    }
    serde_json::from_str(frame).map_err(|e| NaviError::Protocol(format!("invalid IPC frame: {e}")))
}

/// Calls the Unix socket RPC endpoint at `socket_path` with a JSON payload.
///
/// The request is written as one NDJSON line and exactly one response line is
/// read back. `timeout` applies to both the write and the read; a zero timeout
/// puts the stream in nonblocking mode, as Python's `settimeout(0)` does.
pub fn rpc_call(socket_path: &Path, payload: &Value, timeout: Duration) -> NaviResult<Value> {
    let frame = encode_ndjson_line(payload)?;
    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        NaviError::Transport(format!("cannot connect to {}: {e}", socket_path.display()))
    })?;

    configure_timeouts(&mut stream, timeout)?;
    stream
        .write_all(frame.as_bytes())
        .map_err(|e| transport_error("cannot send IPC request", e))?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream).take(MAX_RESPONSE_BYTES);
    match reader.read_line(&mut line) {
        Ok(_) => {}
        Err(error)
            if !line.is_empty()
                && matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                        | ErrorKind::UnexpectedEof
                ) =>
        {
            // A daemon that closes the socket while the request is still unread
            // makes Linux report a reset after delivering the response bytes.
            // The collected line is the whole frame, so treat this like EOF;
            // Python's `readline` likewise accepts a final line without `\n`.
        }
        Err(error) => return Err(transport_error("cannot read IPC response", error)),
    }

    if !line.ends_with('\n') && u64::try_from(line.len()).unwrap_or(u64::MAX) >= MAX_RESPONSE_BYTES
    {
        return Err(NaviError::Protocol(
            "IPC response frame exceeds 4 MiB".into(),
        ));
    }

    let response: IpcResponse = decode_ndjson_line(&line)?;
    response.into_result()
}

/// Applies the same timeout to both directions; zero means nonblocking.
fn configure_timeouts(stream: &mut UnixStream, timeout: Duration) -> NaviResult<()> {
    if timeout.is_zero() {
        stream
            .set_nonblocking(true)
            .map_err(|e| transport_error("cannot set nonblocking mode", e))?;
    } else {
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| transport_error("cannot set read timeout", e))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|e| transport_error("cannot set write timeout", e))?;
    }
    Ok(())
}

/// Maps socket I/O failures onto the workspace error taxonomy.
fn transport_error(context: &str, error: std::io::Error) -> NaviError {
    match error.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => {
            NaviError::Timeout(format!("{context}: {error}"))
        }
        ErrorKind::InvalidData => NaviError::Protocol(format!("{context}: {error}")),
        _ => NaviError::Transport(format!("{context}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// Unique socket path with a guard that unlinks it on drop.
    struct TempSocket(PathBuf);

    impl TempSocket {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "navi-ipc-{label}-{}-{nanos}-{seq}.sock",
                std::process::id()
            )))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn encode_ndjson_line_is_compact_and_newline_terminated() {
        let frame = encode_ndjson_line(&json!({"op": "status", "params": {}}))
            .expect("encoding a Value cannot fail");
        assert_eq!(frame, "{\"op\":\"status\",\"params\":{}}\n");
        assert_eq!(frame.bytes().filter(|b| *b == b'\n').count(), 1);
    }

    #[test]
    fn encode_ndjson_line_escapes_embedded_newlines() {
        let value = json!({"text": "first\nsecond"});
        let frame = encode_ndjson_line(&value).expect("encode");
        assert!(!frame[..frame.len() - 1].contains('\n'));
        assert!(frame.contains("\\n"));
        let decoded: Value = decode_ndjson_line(&frame).expect("decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn decode_ndjson_line_accepts_whitespace_and_typed_values() {
        let decoded: Value =
            decode_ndjson_line("  {\"ok\": true, \"result\": 7}  \n").expect("decode Value");
        assert_eq!(decoded, json!({"ok": true, "result": 7}));

        let response: IpcResponse =
            decode_ndjson_line("{\"ok\":false,\"error\":\"nope\"}\n").expect("decode response");
        assert_eq!(response, IpcResponse::failure("nope"));
    }

    #[test]
    fn decode_ndjson_line_rejects_empty_and_malformed_frames() {
        assert!(decode_ndjson_line::<Value>("").is_err());
        assert!(decode_ndjson_line::<Value>("   \n").is_err());
        assert!(decode_ndjson_line::<Value>("{not json}\n").is_err());
        assert!(decode_ndjson_line::<Value>("{}\n{}\n").is_err());
        assert!(decode_ndjson_line::<IpcResponse>("{\"ok\":true}\n{}\n").is_err());
    }

    #[test]
    fn ipc_request_and_response_use_the_python_wire_format() {
        let request = IpcRequest::without_params("status");
        assert_eq!(
            serde_json::to_value(&request).expect("serialize request"),
            json!({"op": "status", "params": {}})
        );
        assert_eq!(
            request.to_frame().expect("frame"),
            "{\"op\":\"status\",\"params\":{}}\n"
        );

        let parsed: IpcRequest =
            serde_json::from_str("{\"op\":\"ping\"}").expect("params defaults to empty object");
        assert_eq!(parsed, IpcRequest::without_params("ping"));

        assert_eq!(
            serde_json::to_value(IpcResponse::success(json!([1, 2]))).expect("serialize ok"),
            json!({"ok": true, "result": [1, 2]})
        );
        assert_eq!(
            serde_json::to_value(IpcResponse::failure("bad")).expect("serialize error"),
            json!({"ok": false, "error": "bad"})
        );
    }

    #[test]
    fn ipc_response_into_result_maps_success_and_failure() {
        assert_eq!(
            IpcResponse::success(json!({"x": 1}))
                .into_result()
                .expect("success"),
            json!({"x": 1})
        );
        let empty = IpcResponse {
            ok: true,
            error: None,
            result: None,
        };
        assert_eq!(empty.into_result().expect("null"), Value::Null);

        let error = IpcResponse::failure("boom")
            .into_result()
            .expect_err("failure");
        assert!(matches!(error, NaviError::Protocol(_)));
        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn rpc_call_roundtrips_success_over_unix_listener() {
        let socket = TempSocket::new("ok");
        let listener = UnixListener::bind(socket.path()).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: IpcRequest = decode_ndjson_line(&line).expect("decode request");
            assert_eq!(request.op, "status");
            assert_eq!(request.params, json!({"sections": ["settings"]}));
            let frame = encode_ndjson_line(&IpcResponse::success(json!({"uptime": 42})))
                .expect("encode response");
            stream.write_all(frame.as_bytes()).expect("write response");
        });

        let payload =
            serde_json::to_value(IpcRequest::new("status", json!({"sections": ["settings"]})))
                .expect("serialize request");
        let result = rpc_call(socket.path(), &payload, Duration::from_secs(2));
        server.join().expect("server thread");
        assert_eq!(result.expect("rpc result"), json!({"uptime": 42}));
    }

    #[test]
    fn rpc_call_returns_protocol_error_for_failed_response() {
        let socket = TempSocket::new("err");
        let listener = UnixListener::bind(socket.path()).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let frame =
                encode_ndjson_line(&IpcResponse::failure("daemon refused")).expect("encode");
            stream.write_all(frame.as_bytes()).expect("write response");
        });

        let error = rpc_call(
            socket.path(),
            &json!({"op": "ping"}),
            Duration::from_secs(2),
        )
        .expect_err("server reported failure");
        server.join().expect("server thread");
        assert!(matches!(error, NaviError::Protocol(_)), "{error:?}");
        assert!(error.to_string().contains("daemon refused"));
    }

    #[test]
    fn rpc_call_times_out_when_server_is_silent() {
        let socket = TempSocket::new("timeout");
        let listener = UnixListener::bind(socket.path()).expect("bind");
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept");
            thread::sleep(Duration::from_millis(400));
        });

        let started = Instant::now();
        let error = rpc_call(
            socket.path(),
            &json!({"op": "ping"}),
            Duration::from_millis(50),
        )
        .expect_err("no response must time out");
        server.join().expect("server thread");
        assert!(matches!(error, NaviError::Timeout(_)), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn rpc_call_reports_transport_error_when_socket_is_missing() {
        let socket = TempSocket::new("missing");
        assert!(!socket.path().exists());
        let error = rpc_call(socket.path(), &json!({}), Duration::from_millis(200))
            .expect_err("no listener at path");
        assert!(matches!(error, NaviError::Transport(_)), "{error:?}");
        assert!(error.to_string().contains("cannot connect"));
    }

    #[test]
    fn rpc_call_rejects_response_above_the_frame_limit() {
        let socket = TempSocket::new("large");
        let listener = UnixListener::bind(socket.path()).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let padding = " ".repeat(MAX_RESPONSE_BYTES as usize);
            let _ = stream.write_all(padding.as_bytes());
        });

        let error = rpc_call(socket.path(), &json!({}), Duration::from_secs(5))
            .expect_err("frame has no newline and fills the cap");
        server.join().expect("server thread");
        assert!(matches!(error, NaviError::Protocol(_)), "{error:?}");
        assert!(error.to_string().contains("4 MiB"));
    }

    #[test]
    fn rpc_call_accepts_response_without_trailing_newline() {
        let socket = TempSocket::new("eof");
        let listener = UnixListener::bind(socket.path()).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .write_all(br#"{"ok":true,"result":"done"}"#)
                .expect("write response");
        });

        let result = rpc_call(socket.path(), &json!({}), Duration::from_secs(2));
        server.join().expect("server thread");
        assert_eq!(result.expect("rpc result"), json!("done"));
    }
}
