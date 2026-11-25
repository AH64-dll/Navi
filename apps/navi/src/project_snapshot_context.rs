//! Preparatory Linux-only daemon context and held project-root identity groundwork.
//!
//! PREPARATORY MODULE ONLY — NOT WIRED TO SNAPSHOT READS, EXECUTION, OR UI:
//! - Inspects directory metadata ONLY via componentwise `libc::open` / `libc::openat`
//!   with `O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`; never opens or reads
//!   project file contents, never writes project source, and never runs project code.
//! - Holds NO approval authority on its own. Constructing a `ProjectSnapshotContext`,
//!   `HeldProjectRoot`, or `ConnectionHandle` grants zero permission to read a
//!   project snapshot or execute candidate tests.
//! - Any mapping from `project_id` to a filesystem path must be caller-owned from
//!   trusted daemon configuration, NEVER a model-supplied path.
//! - Token uniqueness holds ONLY within a single daemon-owned `CheckedTokenIssuer`
//!   instance (two separate issuers each start at 1). Sequential tokens are internal
//!   correlation/lifetime IDs only—neither approval authority nor unguessable
//!   authentication tokens. Daemon restart resets all issuers and MUST invalidate
//!   all pending contexts and approvals.
//! - Per-connection `ConnectionHandle` tokens avoid invalidating unrelated work on
//!   short-lived request sockets. A future authenticated connection owner must revoke
//!   its `ConnectionHandle` on disconnect (and on drop).
//! - Checking `revalidate(...)` before reading CANNOT make filesystem state and
//!   connection liveness atomic across an operation. A future daemon operation must
//!   hold an appropriate owner-scoped lock, keep `HeldProjectRoot` open, and recheck
//!   selection (`project_id`, `project_epoch`), `generation`, connection liveness,
//!   and `HeldProjectRoot::matches_current_path()` before read, before spawn, and
//!   before returning results.
//! - Raw `(device, inode)` pairs are internal only, redacted in `Debug`, and must
//!   never be logged automatically or exposed to models.

#![allow(dead_code)]

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::path::Component;

pub(crate) const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_PATH_COMPONENTS: usize = 64;
pub(crate) const MAX_COMPONENT_BYTES: usize = 255;
pub(crate) const MAX_PROJECT_ID_BYTES: usize = 128;

const ROOT_IDENTITY_DOMAIN_TAG: &[u8] = b"navi-root-identity-v1:";

/// Fail-closed redacted error type (never embeds filesystem paths or dev/ino pairs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotContextError {
    UnsupportedPlatform,
    NotAbsolute,
    RootPathForbidden,
    DotOrDotDotForbidden,
    InvalidPath,
    PathBoundsExceeded,
    NotADirectory,
    UnsafePermissions,
    OpenFailed(io::ErrorKind),
    InvalidProjectId,
    InvalidToken,
    TokenCounterExhausted,
    ConnectionRevoked,
    ContextMismatch,
    RootChanged,
}

impl fmt::Display for SnapshotContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("componentwise Linux directory-handle opening is unavailable on this platform")
            }
            Self::NotAbsolute => f.write_str("project root path must be absolute"),
            Self::RootPathForbidden => f.write_str("filesystem root '/' is forbidden as a project root"),
            Self::DotOrDotDotForbidden => f.write_str("'.' and '..' path segments are forbidden"),
            Self::InvalidPath => f.write_str("project root path is not canonical normal form"),
            Self::PathBoundsExceeded => f.write_str("project root path exceeds component or byte bounds"),
            Self::NotADirectory => f.write_str("project root path is not a directory"),
            Self::UnsafePermissions => f.write_str("project root or ancestor failed owner/permission safety checks"),
            Self::OpenFailed(_) => f.write_str("componentwise directory open failed"),
            Self::InvalidProjectId => f.write_str("invalid project identifier"),
            Self::InvalidToken => f.write_str("zero or unissued token is invalid"),
            Self::TokenCounterExhausted => f.write_str("monotonic token issuer exhausted without wrap"),
            Self::ConnectionRevoked => f.write_str("owner connection lifetime token was revoked"),
            Self::ContextMismatch => f.write_str("snapshot approval context no longer matches live state"),
            Self::RootChanged => f.write_str("project root path no longer matches held root identity"),
        }
    }
}

impl std::error::Error for SnapshotContextError {}

/// Internal root directory identity derived from `fstat` on a held directory FD.
/// Fields stay private and `Debug` is redacted so raw pairs are never logged
/// automatically or exposed to models.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootIdentity {
    device: u64,
    inode: u64,
}

impl RootIdentity {
    /// Pass an exact domain-separated byte slice (`ROOT_IDENTITY_DOMAIN_TAG`
    /// followed by big-endian `device` and `inode`) to a caller-supplied verified
    /// digest function (e.g., SHA-256) without exposing source paths or adding an
    /// unverified local hash implementation.
    pub(crate) fn digest_with<T, F>(&self, hasher: F) -> T
    where
        F: FnOnce(&[u8]) -> T,
    {
        let mut payload = Vec::with_capacity(ROOT_IDENTITY_DOMAIN_TAG.len() + 16);
        payload.extend_from_slice(ROOT_IDENTITY_DOMAIN_TAG);
        payload.extend_from_slice(&self.device.to_be_bytes());
        payload.extend_from_slice(&self.inode.to_be_bytes());
        hasher(&payload)
    }
}

impl fmt::Debug for RootIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RootIdentity([redacted])")
    }
}

/// Non-zero per-connection lifetime token issued within one daemon-owned issuer.
/// Not an authentication secret or approval authority.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ConnectionToken(u64);

impl ConnectionToken {
    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

/// Non-zero project-selection epoch issued within one daemon-owned issuer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SelectionEpoch(u64);

impl SelectionEpoch {
    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

/// Checked monotonic issuer for non-zero connection or selection tokens.
/// Holds no global state; refuses overflow at `u64::MAX` without wrapping.
/// Uniqueness holds ONLY within one daemon-owned instance for one daemon lifetime.
#[derive(Debug)]
pub(crate) struct CheckedTokenIssuer {
    last_issued: AtomicU64,
}

impl CheckedTokenIssuer {
    pub(crate) fn new() -> Self {
        Self {
            last_issued: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    fn with_last_issued(last_issued: u64) -> Self {
        Self {
            last_issued: AtomicU64::new(last_issued),
        }
    }

    fn issue_raw(&self) -> Result<u64, SnapshotContextError> {
        let mut last = self.last_issued.load(Ordering::SeqCst);
        loop {
            let next = last
                .checked_add(1)
                .ok_or(SnapshotContextError::TokenCounterExhausted)?;
            match self.last_issued.compare_exchange_weak(
                last,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(next),
                Err(observed) => last = observed,
            }
        }
    }

    pub(crate) fn issue_connection_token(&self) -> Result<ConnectionToken, SnapshotContextError> {
        self.issue_raw().map(ConnectionToken)
    }

    pub(crate) fn issue_selection_epoch(&self) -> Result<SelectionEpoch, SnapshotContextError> {
        self.issue_raw().map(SelectionEpoch)
    }
}

/// Read-only observer for one authenticated connection's liveness state.
/// Cannot revoke or forge a connection handle; grants no authority.
#[derive(Clone, Debug)]
pub(crate) struct ConnectionLiveness {
    token: ConnectionToken,
    active: Arc<AtomicBool>,
}

impl ConnectionLiveness {
    pub(crate) fn token(&self) -> ConnectionToken {
        self.token
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

/// Per-connection liveness handle owned by one authenticated owner connection.
/// Revoking or dropping this handle invalidates only this connection's token,
/// without disturbing unrelated short-lived request sockets or other connections.
#[derive(Debug)]
pub(crate) struct ConnectionHandle {
    token: ConnectionToken,
    active: Arc<AtomicBool>,
}

impl ConnectionHandle {
    pub(crate) fn open(issuer: &CheckedTokenIssuer) -> Result<Self, SnapshotContextError> {
        let token = issuer.issue_connection_token()?;
        Ok(Self {
            token,
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    pub(crate) fn token(&self) -> ConnectionToken {
        self.token
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) fn liveness(&self) -> ConnectionLiveness {
        ConnectionLiveness {
            token: self.token,
            active: Arc::clone(&self.active),
        }
    }

    pub(crate) fn revoke(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

impl Drop for ConnectionHandle {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Held read-only directory FD for a componentwise-verified project root.
/// Retains the original `OwnedFd` and `(device, inode)` identity even if the
/// path is later renamed or replaced on disk. Intentionally does NOT implement
/// `PartialEq` over the underlying file descriptor.
pub(crate) struct HeldProjectRoot {
    #[cfg(target_os = "linux")]
    fd: OwnedFd,
    path: PathBuf,
    identity: RootIdentity,
}

impl fmt::Debug for HeldProjectRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HeldProjectRoot([redacted])")
    }
}

impl HeldProjectRoot {
    /// Open `path` componentwise without following symlinks, verifying owner EUID
    /// and non-group/world-writable (`mode & 0o022 == 0`) safety on every component.
    /// Never opens or reads file contents.
    pub(crate) fn open(path: &Path) -> Result<Self, SnapshotContextError> {
        platform::open_verified_root(path)
    }

    pub(crate) fn identity(&self) -> RootIdentity {
        self.identity
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Reopen the original path componentwise and compare its current root
    /// `(device, inode)` identity against this held handle.
    ///
    /// Returns `Ok(true)` ONLY when the path still resolves componentwise (with
    /// all symlink/permission checks intact) to the exact same `(device, inode)`.
    /// Returns `Ok(false)` if replaced by another valid directory, and `Err(_)`
    /// if missing, replaced by a symlink/non-directory, or permissions degraded.
    pub(crate) fn matches_current_path(&self) -> Result<bool, SnapshotContextError> {
        let reopened = platform::open_verified_root(&self.path)?;
        Ok(reopened.identity == self.identity)
    }
}

/// Immutable daemon-owned snapshot approval context.
///
/// Grants NO authority by itself. Checking `revalidate(...)` before reading does
/// not make filesystem and connection state atomic; future daemon routes must
/// hold an owner-scoped lock, keep `HeldProjectRoot` open, and recheck before
/// read, before spawn, and before return.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProjectSnapshotContext {
    project_id: String,
    generation: u64,
    project_epoch: SelectionEpoch,
    connection_token: ConnectionToken,
    root_identity: RootIdentity,
}

impl fmt::Debug for ProjectSnapshotContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectSnapshotContext")
            .field("project_id", &self.project_id)
            .field("generation", &self.generation)
            .field("project_epoch", &self.project_epoch)
            .field("connection_token", &self.connection_token)
            .field("root_identity", &self.root_identity)
            .finish()
    }
}

impl ProjectSnapshotContext {
    pub(crate) fn new(
        project_id: &str,
        generation: u64,
        project_epoch: SelectionEpoch,
        connection_token: ConnectionToken,
        held_root: &HeldProjectRoot,
    ) -> Result<Self, SnapshotContextError> {
        validate_project_id(project_id)?;
        if project_epoch.0 == 0 || connection_token.0 == 0 {
            return Err(SnapshotContextError::InvalidToken);
        }
        Ok(Self {
            project_id: project_id.to_owned(),
            generation,
            project_epoch,
            connection_token,
            root_identity: held_root.identity(),
        })
    }

    pub(crate) fn project_id(&self) -> &str {
        &self.project_id
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn project_epoch(&self) -> SelectionEpoch {
        self.project_epoch
    }

    pub(crate) fn connection_token(&self) -> ConnectionToken {
        self.connection_token
    }

    pub(crate) fn root_identity(&self) -> RootIdentity {
        self.root_identity
    }

    /// Strict fail-closed equality check across every bound field.
    pub(crate) fn matches_exact(&self, other: &Self) -> bool {
        self.project_id == other.project_id
            && self.generation == other.generation
            && self.project_epoch == other.project_epoch
            && self.connection_token == other.connection_token
            && self.root_identity == other.root_identity
    }

    /// Point-in-time fail-closed revalidation of connection liveness, exact
    /// context fields, and live root path identity against the held FD.
    /// Non-atomic against concurrent external filesystem edits: callers must
    /// hold owner-scoped synchronization and recheck before read, spawn, and return.
    pub(crate) fn revalidate(
        &self,
        live_context: &Self,
        held_root: &HeldProjectRoot,
        connection: &ConnectionLiveness,
    ) -> Result<(), SnapshotContextError> {
        if !connection.is_active() {
            return Err(SnapshotContextError::ConnectionRevoked);
        }
        if connection.token() != self.connection_token {
            return Err(SnapshotContextError::ContextMismatch);
        }
        if !self.matches_exact(live_context) || held_root.identity() != self.root_identity {
            return Err(SnapshotContextError::ContextMismatch);
        }
        if !held_root.matches_current_path()? {
            return Err(SnapshotContextError::RootChanged);
        }
        Ok(())
    }
}

fn validate_project_id(project_id: &str) -> Result<(), SnapshotContextError> {
    if project_id.is_empty()
        || project_id.len() > MAX_PROJECT_ID_BYTES
        || project_id.trim() != project_id
    {
        return Err(SnapshotContextError::InvalidProjectId);
    }
    for ch in project_id.chars() {
        let code = ch as u32;
        if code <= 0x1f
            || code == 0x7f
            || (0x202a..=0x202e).contains(&code)
            || (0x2066..=0x2069).contains(&code)
        {
            return Err(SnapshotContextError::InvalidProjectId);
        }
    }
    Ok(())
}

fn validate_dir_permissions(
    uid: u32,
    mode: u32,
    owner_uid: u32,
    is_final: bool,
) -> Result<(), SnapshotContextError> {
    // Stricter than current single-file ProjectReader: reject both group-writable
    // (0o020) and world-writable (0o002) ancestors and final project roots.
    if mode & 0o022 != 0 {
        return Err(SnapshotContextError::UnsafePermissions);
    }
    if is_final {
        if uid != owner_uid {
            return Err(SnapshotContextError::UnsafePermissions);
        }
    } else if uid != 0 && uid != owner_uid {
        return Err(SnapshotContextError::UnsafePermissions);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn parse_canonical_absolute_components(path: &Path) -> Result<Vec<Vec<u8>>, SnapshotContextError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(SnapshotContextError::InvalidPath);
    }
    if bytes.len() > MAX_PATH_BYTES {
        return Err(SnapshotContextError::PathBoundsExceeded);
    }
    if bytes.contains(&0) {
        return Err(SnapshotContextError::InvalidPath);
    }
    if bytes[0] != b'/' {
        return Err(SnapshotContextError::NotAbsolute);
    }
    if bytes == b"/" {
        return Err(SnapshotContextError::RootPathForbidden);
    }
    if bytes.ends_with(b"/") {
        return Err(SnapshotContextError::InvalidPath);
    }

    let mut segments = Vec::new();
    for raw in bytes[1..].split(|&b| b == b'/') {
        if raw.is_empty() {
            return Err(SnapshotContextError::InvalidPath);
        }
        if raw == b"." || raw == b".." {
            return Err(SnapshotContextError::DotOrDotDotForbidden);
        }
        if raw.len() > MAX_COMPONENT_BYTES {
            return Err(SnapshotContextError::PathBoundsExceeded);
        }
        segments.push(raw.to_vec());
        if segments.len() > MAX_PATH_COMPONENTS {
            return Err(SnapshotContextError::PathBoundsExceeded);
        }
    }

    let mut comps = path.components();
    if comps.next() != Some(Component::RootDir) {
        return Err(SnapshotContextError::NotAbsolute);
    }
    let mut normal_count = 0usize;
    for comp in comps {
        match comp {
            Component::Normal(_) => normal_count += 1,
            Component::CurDir | Component::ParentDir => {
                return Err(SnapshotContextError::DotOrDotDotForbidden);
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(SnapshotContextError::InvalidPath);
            }
        }
    }
    if normal_count == 0 || normal_count != segments.len() {
        return Err(SnapshotContextError::InvalidPath);
    }

    Ok(segments)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn current_euid() -> libc::uid_t {
        // SAFETY: `libc::geteuid` takes no pointers and has no failure mode.
        unsafe { libc::geteuid() }
    }

    pub(super) fn open_verified_root(path: &Path) -> Result<HeldProjectRoot, SnapshotContextError> {
        let components = parse_canonical_absolute_components(path)?;
        let owner_uid = current_euid();

        let root_raw = unsafe {
            libc::open(
                b"/\0".as_ptr().cast::<libc::c_char>(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if root_raw < 0 {
            return Err(SnapshotContextError::OpenFailed(
                io::Error::last_os_error().kind(),
            ));
        }
        // SAFETY: `root_raw >= 0` is a freshly acquired owned file descriptor.
        let mut cur_file = unsafe { File::from_raw_fd(root_raw) };
        let root_meta = cur_file
            .metadata()
            .map_err(|err| SnapshotContextError::OpenFailed(err.kind()))?;
        if !root_meta.is_dir() {
            return Err(SnapshotContextError::NotADirectory);
        }
        validate_dir_permissions(root_meta.uid(), root_meta.mode(), owner_uid, false)?;

        let mut last_identity = RootIdentity {
            device: root_meta.dev(),
            inode: root_meta.ino(),
        };

        let last_index = components.len() - 1;
        for (idx, seg) in components.iter().enumerate() {
            let c_seg = CString::new(seg.as_slice())
                .map_err(|_| SnapshotContextError::InvalidPath)?;
            let next_raw = unsafe {
                libc::openat(
                    cur_file.as_raw_fd(),
                    c_seg.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if next_raw < 0 {
                return Err(SnapshotContextError::OpenFailed(
                    io::Error::last_os_error().kind(),
                ));
            }
            // SAFETY: `next_raw >= 0` is a freshly acquired owned file descriptor.
            let next_file = unsafe { File::from_raw_fd(next_raw) };
            let meta = next_file
                .metadata()
                .map_err(|err| SnapshotContextError::OpenFailed(err.kind()))?;
            if !meta.is_dir() {
                return Err(SnapshotContextError::NotADirectory);
            }
            validate_dir_permissions(meta.uid(), meta.mode(), owner_uid, idx == last_index)?;
            last_identity = RootIdentity {
                device: meta.dev(),
                inode: meta.ino(),
            };
            cur_file = next_file;
        }

        let fd = OwnedFd::from(cur_file);
        Ok(HeldProjectRoot {
            fd,
            path: path.to_path_buf(),
            identity: last_identity,
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;

    pub(super) fn open_verified_root(_path: &Path) -> Result<HeldProjectRoot, SnapshotContextError> {
        Err(SnapshotContextError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::{symlink, PermissionsExt};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(label: &str) -> Self {
            let parent = find_safe_test_parent();
            let seq = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
            let path = parent.join(format!(
                ".navi-snap-ctx-test-{}-{}-{}",
                std::process::id(),
                label,
                seq
            ));
            mkdir_private(&path);
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            #[cfg(target_os = "linux")]
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn mkdir_private(path: &Path) {
        fs::create_dir(path).expect("create private synthetic dir");
        #[cfg(target_os = "linux")]
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("chmod 0700 synthetic dir");
    }

    fn find_safe_test_parent() -> PathBuf {
        let mut candidates = Vec::new();
        candidates.push(std::env::temp_dir());
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd);
        }
        for candidate in candidates {
            if candidate.is_dir() && HeldProjectRoot::open(&candidate).is_ok() {
                return candidate;
            }
        }
        panic!(
            "no owner-safe non-group/world-writable parent directory available for synthetic tests"
        );
    }

    #[cfg(target_os = "linux")]
    fn has_cloexec(fd: BorrowedFd<'_>) -> bool {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        flags >= 0 && (flags & libc::FD_CLOEXEC) != 0
    }

    #[test]
    fn regular_root_opens_with_cloexec_and_matches_current_path() {
        let tmp = TempTestDir::new("regular");
        let root = tmp.path().join("proj");
        mkdir_private(&root);

        let held = HeldProjectRoot::open(&root).expect("open regular owner root");
        assert!(has_cloexec(held.as_fd()));
        assert_eq!(held.matches_current_path(), Ok(true));
    }

    #[test]
    fn rename_and_new_directory_returns_false_while_held_fd_retains_old_identity() {
        let tmp = TempTestDir::new("rename");
        let root = tmp.path().join("proj");
        let moved = tmp.path().join("proj-moved");
        mkdir_private(&root);

        let held = HeldProjectRoot::open(&root).unwrap();
        let original_identity = held.identity();
        assert_eq!(held.matches_current_path(), Ok(true));

        fs::rename(&root, &moved).unwrap();
        mkdir_private(&root);

        assert_eq!(held.matches_current_path(), Ok(false));
        assert_eq!(held.identity(), original_identity);

        let moved_held = HeldProjectRoot::open(&moved).unwrap();
        let replacement_held = HeldProjectRoot::open(&root).unwrap();
        assert_eq!(held.identity(), moved_held.identity());
        assert_ne!(held.identity(), replacement_held.identity());
    }

    #[test]
    fn rejects_component_symlink_root_symlink_and_symlink_swap() {
        let tmp = TempTestDir::new("symlink");
        let real_dir = tmp.path().join("real");
        let nested = real_dir.join("sub");
        mkdir_private(&real_dir);
        mkdir_private(&nested);

        // Root itself is a symlink
        let root_link = tmp.path().join("root-link");
        symlink(&real_dir, &root_link).unwrap();
        assert!(matches!(
            HeldProjectRoot::open(&root_link),
            Err(SnapshotContextError::OpenFailed(_))
        ));

        // Intermediate component is a symlink
        let via_component_link = root_link.join("sub");
        assert!(matches!(
            HeldProjectRoot::open(&via_component_link),
            Err(SnapshotContextError::OpenFailed(_))
        ));

        // Directory swapped to a symlink after open fails closed on matches_current_path
        let swap_target = tmp.path().join("swap-target");
        let moved = tmp.path().join("swap-moved");
        mkdir_private(&swap_target);
        let held = HeldProjectRoot::open(&swap_target).unwrap();
        fs::rename(&swap_target, &moved).unwrap();
        symlink(&moved, &swap_target).unwrap();
        assert!(matches!(
            held.matches_current_path(),
            Err(SnapshotContextError::OpenFailed(_))
        ));
    }

    #[test]
    fn rejects_dotdot_curdir_nonabsolute_root_path_and_non_directory() {
        let tmp = TempTestDir::new("invalid-paths");
        let dir = tmp.path().join("dir");
        mkdir_private(&dir);
        let regular_file = tmp.path().join("not-a-dir.txt");
        fs::write(&regular_file, b"text").unwrap();

        assert_eq!(
            HeldProjectRoot::open(Path::new("/")).unwrap_err(),
            SnapshotContextError::RootPathForbidden
        );
        assert_eq!(
            HeldProjectRoot::open(Path::new("relative/proj")).unwrap_err(),
            SnapshotContextError::NotAbsolute
        );
        assert_eq!(
            HeldProjectRoot::open(Path::new("")).unwrap_err(),
            SnapshotContextError::InvalidPath
        );
        assert_eq!(
            HeldProjectRoot::open(&dir.join("..").join("dir")).unwrap_err(),
            SnapshotContextError::DotOrDotDotForbidden
        );
        assert_eq!(
            HeldProjectRoot::open(&PathBuf::from(format!("{}/./dir", tmp.path().display())))
                .unwrap_err(),
            SnapshotContextError::DotOrDotDotForbidden
        );
        assert_eq!(
            HeldProjectRoot::open(&PathBuf::from(format!("{}//dir", tmp.path().display())))
                .unwrap_err(),
            SnapshotContextError::InvalidPath
        );
        assert_eq!(
            HeldProjectRoot::open(&PathBuf::from(format!("{}/", dir.display()))).unwrap_err(),
            SnapshotContextError::InvalidPath
        );
        // Non-directory leaf file rejected by O_DIRECTORY
        assert!(matches!(
            HeldProjectRoot::open(&regular_file),
            Err(SnapshotContextError::OpenFailed(_))
        ));
    }

    #[test]
    fn rejects_group_and_world_writable_and_wrong_owner_uid() {
        // Matrix covering root/owner/foreign UIDs and group/world-writable modes (0o022)
        assert_eq!(validate_dir_permissions(0, 0o755, 1000, false), Ok(()));
        assert_eq!(validate_dir_permissions(1000, 0o750, 1000, false), Ok(()));
        assert_eq!(validate_dir_permissions(1000, 0o700, 1000, true), Ok(()));
        assert_eq!(
            validate_dir_permissions(0, 0o775, 1000, false),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(1000, 0o770, 1000, false),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(1000, 0o770, 1000, true),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(1000, 0o720, 1000, true),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(2000, 0o755, 1000, false),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(0, 0o1777, 1000, false),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(0, 0o755, 1000, true),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(2000, 0o700, 1000, true),
            Err(SnapshotContextError::UnsafePermissions)
        );
        assert_eq!(
            validate_dir_permissions(1000, 0o702, 1000, true),
            Err(SnapshotContextError::UnsafePermissions)
        );

        // Live group-writable and world-writable refusal on both root and ancestor
        let tmp = TempTestDir::new("perms");
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        mkdir_private(&parent);
        mkdir_private(&child);

        for unsafe_mode in [0o770, 0o720, 0o702, 0o777] {
            // Unsafe final root
            fs::set_permissions(&child, fs::Permissions::from_mode(unsafe_mode)).unwrap();
            assert_eq!(
                HeldProjectRoot::open(&child).unwrap_err(),
                SnapshotContextError::UnsafePermissions
            );
            fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

            // Unsafe ancestor
            fs::set_permissions(&parent, fs::Permissions::from_mode(unsafe_mode)).unwrap();
            assert_eq!(
                HeldProjectRoot::open(&child).unwrap_err(),
                SnapshotContextError::UnsafePermissions
            );
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        }

        // System root-owned directory rejected as final project root for non-root EUID
        if platform::current_euid() != 0 && Path::new("/usr").is_dir() {
            assert_eq!(
                HeldProjectRoot::open(Path::new("/usr")).unwrap_err(),
                SnapshotContextError::UnsafePermissions
            );
        }
    }

    #[test]
    fn rejects_overlong_and_overdeep_paths_and_components() {
        let deep = format!(
            "/{}",
            (0..=MAX_PATH_COMPONENTS)
                .map(|i| format!("d{i}"))
                .collect::<Vec<_>>()
                .join("/")
        );
        assert_eq!(
            HeldProjectRoot::open(Path::new(&deep)).unwrap_err(),
            SnapshotContextError::PathBoundsExceeded
        );

        let long_seg = format!("/{}", "a".repeat(MAX_COMPONENT_BYTES + 1));
        assert_eq!(
            HeldProjectRoot::open(Path::new(&long_seg)).unwrap_err(),
            SnapshotContextError::PathBoundsExceeded
        );

        let seg200 = "b".repeat(200);
        let overlong_path = format!(
            "/{}",
            (0..21).map(|_| seg200.as_str()).collect::<Vec<_>>().join("/")
        );
        assert!(overlong_path.len() > MAX_PATH_BYTES);
        assert_eq!(
            HeldProjectRoot::open(Path::new(&overlong_path)).unwrap_err(),
            SnapshotContextError::PathBoundsExceeded
        );
    }

    #[test]
    fn token_issuer_produces_distinct_monotonic_ids_and_refuses_overflow() {
        let issuer = CheckedTokenIssuer::new();
        let c1 = issuer.issue_connection_token().unwrap();
        let c2 = issuer.issue_connection_token().unwrap();
        let s3 = issuer.issue_selection_epoch().unwrap();
        assert_eq!(c1.value(), 1);
        assert_eq!(c2.value(), 2);
        assert_eq!(s3.value(), 3);
        assert_ne!(c1, c2);

        // Two independent issuers each start at 1 (uniqueness is per daemon-owned issuer only).
        let separate_issuer = CheckedTokenIssuer::new();
        assert_eq!(separate_issuer.issue_connection_token().unwrap().value(), 1);

        let near_max = CheckedTokenIssuer::with_last_issued(u64::MAX - 1);
        let last = near_max.issue_connection_token().unwrap();
        assert_eq!(last.value(), u64::MAX);
        assert_eq!(
            near_max.issue_connection_token(),
            Err(SnapshotContextError::TokenCounterExhausted)
        );
        assert_eq!(
            near_max.issue_selection_epoch(),
            Err(SnapshotContextError::TokenCounterExhausted)
        );
    }

    #[test]
    fn connection_handle_revocation_is_per_connection_and_revokes_on_drop() {
        let issuer = CheckedTokenIssuer::new();
        let conn_a = ConnectionHandle::open(&issuer).unwrap();
        let conn_b = ConnectionHandle::open(&issuer).unwrap();
        let live_a = conn_a.liveness();
        let live_b = conn_b.liveness();
        assert_ne!(conn_a.token(), conn_b.token());
        assert!(live_a.is_active());
        assert!(live_b.is_active());

        // Revoking conn_a does not affect conn_b
        conn_a.revoke();
        assert!(!conn_a.is_active());
        assert!(!live_a.is_active());
        assert!(live_b.is_active());

        // Scope drop automatically revokes the connection via Drop
        let live_c = {
            let conn_c = ConnectionHandle::open(&issuer).unwrap();
            let obs = conn_c.liveness();
            assert!(obs.is_active());
            obs
        };
        assert!(!live_c.is_active());
    }

    #[test]
    fn context_comparison_and_revalidation_fail_closed_on_any_field_or_root_drift() {
        let tmp = TempTestDir::new("context");
        let root_a = tmp.path().join("proj-a");
        let root_b = tmp.path().join("proj-b");
        mkdir_private(&root_a);
        mkdir_private(&root_b);

        let held_a = HeldProjectRoot::open(&root_a).unwrap();
        let held_b = HeldProjectRoot::open(&root_b).unwrap();

        let conn_issuer = CheckedTokenIssuer::new();
        let sel_issuer = CheckedTokenIssuer::new();
        let conn_1 = ConnectionHandle::open(&conn_issuer).unwrap();
        let conn_2 = ConnectionHandle::open(&conn_issuer).unwrap();
        let live_1 = conn_1.liveness();
        let live_2 = conn_2.liveness();
        let epoch_1 = sel_issuer.issue_selection_epoch().unwrap();
        let epoch_2 = sel_issuer.issue_selection_epoch().unwrap();

        for bad_id in [
            "",
            " lead",
            "bad\nid",
            "bad\u{202e}id",
            &"x".repeat(MAX_PROJECT_ID_BYTES + 1),
        ] {
            assert_eq!(
                ProjectSnapshotContext::new(bad_id, 1, epoch_1, conn_1.token(), &held_a)
                    .unwrap_err(),
                SnapshotContextError::InvalidProjectId
            );
        }
        assert_eq!(
            ProjectSnapshotContext::new("proj-a", 1, SelectionEpoch(0), conn_1.token(), &held_a)
                .unwrap_err(),
            SnapshotContextError::InvalidToken
        );
        assert_eq!(
            ProjectSnapshotContext::new("proj-a", 1, epoch_1, ConnectionToken(0), &held_a)
                .unwrap_err(),
            SnapshotContextError::InvalidToken
        );

        let base =
            ProjectSnapshotContext::new("proj-a", 5, epoch_1, conn_1.token(), &held_a).unwrap();
        assert!(base.matches_exact(&base));
        assert_eq!(base.revalidate(&base, &held_a, &live_1), Ok(()));

        // Any single field drift fails both matches_exact and revalidate
        let drifted = [
            ProjectSnapshotContext::new("proj-b", 5, epoch_1, conn_1.token(), &held_a).unwrap(),
            ProjectSnapshotContext::new("proj-a", 6, epoch_1, conn_1.token(), &held_a).unwrap(),
            ProjectSnapshotContext::new("proj-a", 5, epoch_2, conn_1.token(), &held_a).unwrap(),
            ProjectSnapshotContext::new("proj-a", 5, epoch_1, conn_2.token(), &held_a).unwrap(),
            ProjectSnapshotContext::new("proj-a", 5, epoch_1, conn_1.token(), &held_b).unwrap(),
        ];
        for other in &drifted {
            assert!(!base.matches_exact(other));
            assert_eq!(
                base.revalidate(other, &held_a, &live_1),
                Err(SnapshotContextError::ContextMismatch)
            );
        }

        // Wrong connection observer or wrong held root rejected
        assert_eq!(
            base.revalidate(&base, &held_a, &live_2),
            Err(SnapshotContextError::ContextMismatch)
        );
        assert_eq!(
            base.revalidate(&base, &held_b, &live_1),
            Err(SnapshotContextError::ContextMismatch)
        );

        // Replacing root on disk causes revalidation to fail with RootChanged
        let moved_a = tmp.path().join("proj-a-moved");
        fs::rename(&root_a, &moved_a).unwrap();
        mkdir_private(&root_a);
        assert_eq!(
            base.revalidate(&base, &held_a, &live_1),
            Err(SnapshotContextError::RootChanged)
        );

        // Revoking connection causes revalidation to fail with ConnectionRevoked
        conn_1.revoke();
        assert_eq!(
            base.revalidate(&base, &held_a, &live_1),
            Err(SnapshotContextError::ConnectionRevoked)
        );
    }

    #[test]
    fn root_identity_debug_is_redacted_and_digest_callback_is_domain_separated() {
        let tmp = TempTestDir::new("redact");
        let root = tmp.path().join("secret-repo-name");
        mkdir_private(&root);
        let held = HeldProjectRoot::open(&root).unwrap();
        let id = held.identity();

        let id_dbg = format!("{:?}", id);
        let held_dbg = format!("{:?}", held);
        assert_eq!(id_dbg, "RootIdentity([redacted])");
        assert_eq!(held_dbg, "HeldProjectRoot([redacted])");
        assert!(!id_dbg.contains(&id.device.to_string()));
        assert!(!id_dbg.contains(&id.inode.to_string()));
        assert!(!held_dbg.contains("secret-repo-name"));

        let payload = id.digest_with(|bytes| bytes.to_vec());
        let tag_len = ROOT_IDENTITY_DOMAIN_TAG.len();
        assert_eq!(payload.len(), tag_len + 16);
        assert_eq!(&payload[..tag_len], ROOT_IDENTITY_DOMAIN_TAG);
        assert_eq!(&payload[tag_len..tag_len + 8], &id.device.to_be_bytes());
        assert_eq!(&payload[tag_len + 8..], &id.inode.to_be_bytes());
    }
}