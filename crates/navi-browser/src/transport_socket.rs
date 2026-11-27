//! Private POSIX domain-socket transport for the NAVI browser bridge.
//!
//! Ported from `packages/navi/src/transport/{socket,channel,connector}.ts`. The server binds one
//! pathname socket and refuses to overwrite an existing path; a client connects to that exact path.
//! Each accepted or connected stream carries one bounded Native Messaging frame channel: writes are
//! four-byte little-endian length prefixes followed by validated JSON, and reads are decoded
//! incrementally so fragmentation and coalescing are invisible to callers.
//!
//! This is a private-path boundary, not a peer-credential check: any process able to open the
//! socket can use it. The module uses only safe Rust and the standard library's Unix sockets.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use navi_types::{NaviError, NaviResult};

use crate::framing::{NativeMessageDecoder, encode_native_message};
use crate::protocol::{MAX_FRAME_BYTES, ProtocolFrame, decode_frame, encode_frame};

/// Fixed socket file name inside the configured bridge directory.
pub const BRIDGE_SOCKET_NAME: &str = "bridge.sock";

/// Default time allowed for a client to connect to a listening bridge.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Size of one read from the stream. Frames may span any number of reads.
const READ_CHUNK: usize = 8 * 1024;

/// Returns the socket path inside `socket_dir`. There is no discovery protocol.
pub fn bridge_socket_path(socket_dir: &Path) -> PathBuf {
    socket_dir.join(BRIDGE_SOCKET_NAME)
}

fn io_transport_error(action: &str, path: &Path, err: io::Error) -> NaviError {
    NaviError::Transport(format!("{action} {}: {err}", path.display()))
}

fn accept_stream(listener: &UnixListener) -> NaviResult<UnixStream> {
    let (stream, _) = listener
        .accept()
        .map_err(|err| NaviError::Transport(format!("socket accept failed: {err}")))?;
    Ok(stream)
}

/// A bound bridge socket. Dropping the server closes the listener and removes the socket file it
/// created, but only if that file still has the device and inode observed at bind time.
#[derive(Debug)]
pub struct SocketTransportServer {
    listener: UnixListener,
    path: PathBuf,
    identity: Option<(u64, u64)>,
    active: Option<SocketConnection>,
}

impl SocketTransportServer {
    /// Binds a new private socket at `path`.
    ///
    /// An existing path is never unlinked or reused: a stale socket requires explicit removal by
    /// the operator. The new socket is restricted to mode 0600 before it is returned.
    pub fn bind(path: &Path) -> NaviResult<Self> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(NaviError::Transport(format!(
                    "refusing to bind {}: the path already exists; stop the running NAVI CLI and remove a stale socket explicitly",
                    path.display()
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_transport_error("cannot inspect", path, err)),
        }

        let listener =
            UnixListener::bind(path).map_err(|err| io_transport_error("cannot bind", path, err))?;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) => {
                drop(listener);
                let _ = std::fs::remove_file(path);
                return Err(io_transport_error("cannot inspect bound socket", path, err));
            }
        };
        if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            drop(listener);
            let _ = std::fs::remove_file(path);
            return Err(io_transport_error(
                "cannot restrict permissions of",
                path,
                err,
            ));
        }

        Ok(Self {
            listener,
            path: path.to_path_buf(),
            identity: Some((metadata.dev(), metadata.ino())),
            active: None,
        })
    }

    /// Returns the path this server is bound to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts one connection, blocking until a peer arrives.
    ///
    /// POSIX sockets carry no peer address; diagnostics use the bound path instead.
    pub fn accept(&self) -> NaviResult<SocketConnection> {
        let stream = accept_stream(&self.listener)?;
        Ok(SocketConnection::from_stream(stream))
    }

    /// Sends a frame on the server's current connection, accepting one if needed.
    ///
    /// This is a convenience wrapper for callers that drive the server as a peer endpoint directly.
    pub fn send_frame(&mut self, frame: &ProtocolFrame) -> NaviResult<()> {
        self.ensure_active()?;
        self.active
            .as_mut()
            .expect("connection inserted by ensure_active")
            .send_frame(frame)
    }

    /// Receives a frame on the server's current connection, accepting one if needed.
    ///
    /// Like [`SocketConnection::recv_frame`], this blocks until a whole frame or a clean end of
    /// stream is available.
    pub fn recv_frame(&mut self) -> NaviResult<Option<ProtocolFrame>> {
        self.ensure_active()?;
        self.active
            .as_mut()
            .expect("connection inserted by ensure_active")
            .recv_frame()
    }

    fn ensure_active(&mut self) -> NaviResult<()> {
        if self.active.is_none() {
            let stream = accept_stream(&self.listener)?;
            self.active = Some(SocketConnection::from_stream(stream));
        }
        Ok(())
    }
}

impl Drop for SocketTransportServer {
    fn drop(&mut self) {
        let Some((dev, ino)) = self.identity else {
            return;
        };
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket() && metadata.dev() == dev && metadata.ino() == ino {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// One established socket link. Both the accepted server side and the connected client side have
/// the same frame-send and frame-receive capabilities.
#[derive(Debug)]
pub struct SocketConnection {
    stream: UnixStream,
    decoder: NativeMessageDecoder,
    ready: VecDeque<ProtocolFrame>,
    failure: Option<NaviError>,
    eof: bool,
}

/// Client side of a bridge link. It is the same type as an accepted connection: `connect` is the
/// client entry point and `SocketTransportServer::accept` produces the peer handle.
pub type SocketTransportClient = SocketConnection;

impl SocketConnection {
    fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            decoder: NativeMessageDecoder::with_default_limit(),
            ready: VecDeque::new(),
            failure: None,
            eof: false,
        }
    }

    /// Connects to a private bridge socket already listening at `path`, within `timeout`.
    ///
    /// Missing, non-socket, or symlinked paths fail immediately instead of being retried. A connect
    /// that does not complete before the deadline reports [`NaviError::Timeout`].
    pub fn connect(path: &Path, timeout: Duration) -> NaviResult<Self> {
        check_socket_file(path)?;
        let target = path.to_path_buf();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("navi-socket-connect".to_owned())
            .spawn(move || {
                let result = UnixStream::connect(&target);
                let _ = sender.send(result);
            })
            .map_err(|err| {
                NaviError::Transport(format!("could not start socket connect thread: {err}"))
            })?;

        match receiver.recv_timeout(timeout) {
            Ok(Ok(stream)) => Ok(Self::from_stream(stream)),
            Ok(Err(err)) => Err(io_transport_error("cannot connect to", path, err)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(NaviError::Timeout(format!(
                "timed out connecting to {} after {} ms",
                path.display(),
                timeout.as_millis()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(NaviError::Transport(format!(
                "socket connect thread ended for {}",
                path.display()
            ))),
        }
    }

    /// Validates, frames, and writes one protocol frame to the peer.
    ///
    /// A write failure is terminal: every later call on this link returns the stored error.
    pub fn send_frame(&mut self, frame: &ProtocolFrame) -> NaviResult<()> {
        if let Some(err) = &self.failure {
            return Err(err.clone());
        }
        let text = encode_frame(frame)?;
        let bytes = encode_native_message(text.as_bytes(), MAX_FRAME_BYTES)?;
        if let Err(err) = self.stream.write_all(&bytes) {
            let failure = NaviError::Transport(format!("socket write failed: {err}"));
            self.failure = Some(failure.clone());
            return Err(failure);
        }
        Ok(())
    }

    /// Blocks until one complete frame is available, returning `None` at a clean end of stream.
    ///
    /// Partial reads are buffered until the frame is complete. A decode failure, an invalid UTF-8
    /// body, a read error, or a truncated frame at end of stream is terminal and poisons the link.
    pub fn recv_frame(&mut self) -> NaviResult<Option<ProtocolFrame>> {
        loop {
            if let Some(err) = &self.failure {
                return Err(err.clone());
            }
            if let Some(frame) = self.ready.pop_front() {
                return Ok(Some(frame));
            }
            if self.eof {
                return Ok(None);
            }

            let mut buffer = [0_u8; READ_CHUNK];
            let count = match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    if let Err(err) = self.decoder.finish() {
                        self.failure = Some(err.clone());
                        return Err(err);
                    }
                    return Ok(None);
                }
                Ok(count) => count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    let failure = NaviError::Transport(format!("socket read failed: {err}"));
                    self.failure = Some(failure.clone());
                    return Err(failure);
                }
            };

            let bodies = match self.decoder.push(&buffer[..count]) {
                Ok(bodies) => bodies,
                Err(err) => {
                    self.failure = Some(err.clone());
                    return Err(err);
                }
            };
            for body in bodies {
                let text = match std::str::from_utf8(&body) {
                    Ok(text) => text,
                    Err(err) => {
                        let failure =
                            NaviError::Protocol(format!("frame body is not valid UTF-8: {err}"));
                        self.failure = Some(failure.clone());
                        self.ready.clear();
                        return Err(failure);
                    }
                };
                let frame = match decode_frame(text) {
                    Ok(frame) => frame,
                    Err(err) => {
                        self.failure = Some(err.clone());
                        self.ready.clear();
                        return Err(err);
                    }
                };
                self.ready.push_back(frame);
            }
        }
    }
}

fn check_socket_file(path: &Path) -> NaviResult<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|err| io_transport_error("no NAVI socket at", path, err))?;
    if metadata.file_type().is_symlink() {
        return Err(NaviError::Transport(format!(
            "NAVI bridge socket path must not be a symlink: {}",
            path.display()
        )));
    }
    if !metadata.file_type().is_socket() {
        return Err(NaviError::Transport(format!(
            "NAVI bridge socket path is not a socket: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BRIDGE_SOCKET_NAME, DEFAULT_CONNECT_TIMEOUT, SocketTransportClient, SocketTransportServer,
        bridge_socket_path,
    };
    use crate::framing::{DEFAULT_MAX_FRAME_BYTES, encode_native_message};
    use crate::protocol::{
        ByeReason, Capability, NAVI_PROTOCOL_VERSION, PeerInfo, PeerKind, ProtocolFrame,
    };
    use std::io::Write;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_socket_path() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "navi-socket-{}-{stamp}-{id}.sock",
            std::process::id()
        ))
    }

    fn hello(seq: u64) -> ProtocolFrame {
        ProtocolFrame::Hello {
            v: NAVI_PROTOCOL_VERSION,
            seq,
            peer: PeerInfo {
                kind: PeerKind::Cli,
                version: "1.0.0".to_owned(),
            },
            capabilities: vec![Capability::Observe],
        }
    }

    fn bye(seq: u64) -> ProtocolFrame {
        ProtocolFrame::Bye {
            v: NAVI_PROTOCOL_VERSION,
            seq,
            connection_id: "conn-1".to_owned(),
            reason: ByeReason::Closed,
        }
    }

    fn framed_json(frame: &ProtocolFrame) -> Vec<u8> {
        let text = crate::protocol::encode_frame(frame).expect("encode test frame");
        encode_native_message(text.as_bytes(), DEFAULT_MAX_FRAME_BYTES).expect("frame test body")
    }

    #[test]
    fn bound_socket_exchanges_frames_in_both_directions() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("bind");
        let mut client =
            SocketTransportClient::connect(&path, DEFAULT_CONNECT_TIMEOUT).expect("connect");
        let mut peer = server.accept().expect("accept");

        let greeting = hello(1);
        client.send_frame(&greeting).expect("client sends hello");
        assert_eq!(
            peer.recv_frame().expect("server receives hello"),
            Some(greeting)
        );

        let farewell = bye(2);
        peer.send_frame(&farewell).expect("server sends bye");
        assert_eq!(
            client.recv_frame().expect("client receives bye"),
            Some(farewell)
        );
    }

    #[test]
    fn server_can_exchange_frames_without_an_explicit_accept() {
        let path = temp_socket_path();
        let mut server = SocketTransportServer::bind(&path).expect("bind");
        let mut client =
            SocketTransportClient::connect(&path, DEFAULT_CONNECT_TIMEOUT).expect("connect");

        let request = hello(11);
        client.send_frame(&request).expect("client sends");
        assert_eq!(server.recv_frame().expect("server receives"), Some(request));

        let reply = bye(12);
        server.send_frame(&reply).expect("server replies");
        assert_eq!(client.recv_frame().expect("client receives"), Some(reply));
    }

    #[test]
    fn recv_frame_returns_coalesced_frames_in_order() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("bind");
        let mut client =
            SocketTransportClient::connect(&path, DEFAULT_CONNECT_TIMEOUT).expect("connect");
        let mut peer = server.accept().expect("accept");

        for seq in 1..=4 {
            client.send_frame(&hello(seq)).expect("send frame");
        }
        for seq in 1..=4 {
            assert_eq!(peer.recv_frame().expect("recv frame"), Some(hello(seq)));
        }
    }

    #[test]
    fn recv_frame_waits_for_a_fragmented_frame() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("bind");
        let mut raw = UnixStream::connect(&path).expect("raw connect");
        let mut peer = server.accept().expect("accept");
        let bytes = framed_json(&hello(7));
        let split = 3;

        // Keep an owned copy for the writer thread so the prefix slice stays valid here.
        let remainder = bytes[split..].to_vec();
        let mut writer = raw.try_clone().expect("clone raw stream");
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            writer.write_all(&remainder).expect("write remainder");
        });
        raw.write_all(&bytes[..split]).expect("write prefix");

        assert_eq!(
            peer.recv_frame().expect("receive fragmented frame"),
            Some(hello(7)),
            "a partial read must not be reported as end of frames"
        );
        handle.join().expect("writer thread joins");
    }

    #[test]
    fn clean_peer_shutdown_yields_none_repeatedly() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("bind");
        let mut peer = {
            let mut client =
                SocketTransportClient::connect(&path, DEFAULT_CONNECT_TIMEOUT).expect("connect");
            let peer = server.accept().expect("accept");
            client.send_frame(&hello(3)).expect("send before close");
            drop(client);
            peer
        };

        assert_eq!(peer.recv_frame().expect("buffered frame"), Some(hello(3)));
        assert_eq!(peer.recv_frame().expect("clean end"), None);
        assert_eq!(peer.recv_frame().expect("stays ended"), None);
    }

    #[test]
    fn invalid_json_body_fails_the_connection_permanently() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("bind");
        let mut raw = UnixStream::connect(&path).expect("raw connect");
        let mut peer = server.accept().expect("accept");
        let bad = encode_native_message(b"not json", DEFAULT_MAX_FRAME_BYTES).expect("frame");
        raw.write_all(&bad).expect("write bad frame");

        assert!(peer.recv_frame().is_err());
        assert!(
            peer.recv_frame().is_err(),
            "a decoded framing or JSON error is terminal"
        );
    }

    #[test]
    fn truncated_frame_at_eof_is_rejected() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("bind");
        let mut raw = UnixStream::connect(&path).expect("raw connect");
        let mut peer = server.accept().expect("accept");
        let mut bytes = framed_json(&hello(5));
        bytes.truncate(bytes.len() - 2);
        raw.write_all(&bytes).expect("write partial frame");
        drop(raw);

        let error = peer.recv_frame().expect_err("truncated frame is an error");
        assert!(
            matches!(&error, navi_types::NaviError::Protocol(_)),
            "unexpected error: {error}"
        );
        assert!(peer.recv_frame().is_err());
    }

    #[test]
    fn bind_refuses_an_existing_path_without_removing_it() {
        let path = temp_socket_path();
        let server = SocketTransportServer::bind(&path).expect("first bind");
        assert!(SocketTransportServer::bind(&path).is_err());
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("socket metadata")
                .file_type()
                .is_socket(),
            "the original socket is untouched"
        );

        let mut client =
            SocketTransportClient::connect(&path, DEFAULT_CONNECT_TIMEOUT).expect("connect");
        let mut peer = server.accept().expect("accept");
        client.send_frame(&hello(9)).expect("send");
        assert_eq!(peer.recv_frame().expect("recv"), Some(hello(9)));
    }

    #[test]
    fn connect_rejects_missing_and_non_socket_paths() {
        let missing = temp_socket_path();
        assert!(
            SocketTransportClient::connect(&missing, DEFAULT_CONNECT_TIMEOUT).is_err(),
            "a missing socket is not retried"
        );

        let regular = temp_socket_path();
        std::fs::write(&regular, b"not a socket").expect("write regular file");
        assert!(SocketTransportClient::connect(&regular, DEFAULT_CONNECT_TIMEOUT).is_err());
        std::fs::remove_file(&regular).expect("remove regular file");
    }

    #[test]
    fn socket_path_helper_uses_the_fixed_name() {
        let dir = PathBuf::from("/tmp/navi-private-socket-dir");
        let path = bridge_socket_path(&dir);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(BRIDGE_SOCKET_NAME)
        );
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert_eq!(DEFAULT_CONNECT_TIMEOUT, Duration::from_millis(2_000));
    }

    #[test]
    fn bind_restricts_socket_permissions_to_owner_only() {
        let path = temp_socket_path();
        let _server = SocketTransportServer::bind(&path).expect("bind");
        let mode = std::fs::symlink_metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "only the owner may use the private socket");
    }

    #[test]
    fn dropping_the_server_removes_the_socket_file() {
        let path = temp_socket_path();
        {
            let _server = SocketTransportServer::bind(&path).expect("bind");
            assert!(
                std::fs::symlink_metadata(&path).is_ok(),
                "the socket exists while the server is alive"
            );
        }
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "the socket file is removed when the server is dropped"
        );
    }
}
