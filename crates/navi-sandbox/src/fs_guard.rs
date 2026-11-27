//! Workspace-confined filesystem guard for sandbox tool calls.
//!
//! Paths are validated twice: lexically (no `..` or prefix components, and
//! component-wise containment under the canonical root) and physically (the
//! deepest existing ancestor is canonicalized, so an existing symlink cannot
//! redirect a read or write outside the workspace).

use navi_types::{NaviError, NaviResult};
use std::path::{Component, Path, PathBuf};

/// Resolves workspace-relative or absolute paths under a fixed canonical root.
#[derive(Debug, Clone)]
pub struct WorkspaceFsGuard {
    root: PathBuf,
}

impl WorkspaceFsGuard {
    /// Creates a guard rooted at `root`, which must be an existing directory.
    pub fn new(root: PathBuf) -> NaviResult<Self> {
        if root.as_os_str().is_empty() {
            return Err(NaviError::InvalidInput(
                "workspace root must not be empty".into(),
            ));
        }
        let canonical = std::fs::canonicalize(&root).map_err(|err| {
            NaviError::InvalidInput(format!(
                "workspace root '{}' cannot be resolved: {err}",
                root.display()
            ))
        })?;
        if !std::fs::metadata(&canonical)?.is_dir() {
            return Err(NaviError::InvalidInput(format!(
                "workspace root '{}' is not a directory",
                root.display()
            )));
        }
        Ok(Self { root: canonical })
    }

    /// Resolves a path for reading, rejecting traversal and symlink escapes.
    pub fn resolve_read(&self, rel_or_abs: &Path) -> NaviResult<PathBuf> {
        self.resolve(rel_or_abs)
    }

    /// Resolves a path for writing; the final component may not exist yet.
    pub fn resolve_write(&self, rel_or_abs: &Path) -> NaviResult<PathBuf> {
        self.resolve(rel_or_abs)
    }

    /// Verifies that an already-resolved path is confined to the root after
    /// every existing symlink component has been followed.
    pub fn check_no_symlink_escape(&self, path: &Path) -> NaviResult<()> {
        reject_traversal(path)?;
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.confined_candidate(path)?
        };
        if !is_within(&self.root, &candidate) {
            return Err(NaviError::PermissionDenied(format!(
                "path '{}' is outside the workspace root",
                path.display()
            )));
        }
        let _ = self.canonicalize_confined(&candidate)?;
        Ok(())
    }

    /// Requires a regular file or directory that is not group/other accessible
    /// and is owned by the effective user, when that UID can be determined.
    pub fn verify_owner_only_mode(path: &Path) -> NaviResult<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let meta = std::fs::symlink_metadata(path)?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                return Err(NaviError::PermissionDenied(format!(
                    "refusing symlink in owner-only mode check: '{}'",
                    path.display()
                )));
            }
            if !kind.is_file() && !kind.is_dir() {
                return Err(NaviError::PermissionDenied(format!(
                    "not a regular file or directory: '{}'",
                    path.display()
                )));
            }
            if meta.mode() & 0o077 != 0 {
                return Err(NaviError::PermissionDenied(format!(
                    "path '{}' is accessible by group or others",
                    path.display()
                )));
            }
            if current_effective_uid().is_some_and(|uid| meta.uid() != uid) {
                return Err(NaviError::PermissionDenied(format!(
                    "path '{}' is not owned by the current user",
                    path.display()
                )));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(NaviError::InvalidInput(
                "owner-only mode checks require a Unix platform".into(),
            ))
        }
    }

    fn resolve(&self, rel_or_abs: &Path) -> NaviResult<PathBuf> {
        let candidate = self.confined_candidate(rel_or_abs)?;
        self.canonicalize_confined(&candidate)
    }

    fn confined_candidate(&self, rel_or_abs: &Path) -> NaviResult<PathBuf> {
        reject_traversal(rel_or_abs)?;
        let mut candidate = if rel_or_abs.is_absolute() {
            PathBuf::new()
        } else {
            self.root.clone()
        };
        for component in rel_or_abs.components() {
            if !matches!(component, Component::CurDir) {
                candidate.push(component.as_os_str());
            }
        }
        if !is_within(&self.root, &candidate) {
            return Err(NaviError::PermissionDenied(format!(
                "path '{}' is outside the workspace root",
                rel_or_abs.display()
            )));
        }
        Ok(candidate)
    }

    fn canonicalize_confined(&self, candidate: &Path) -> NaviResult<PathBuf> {
        let ancestor = deepest_existing_ancestor(candidate).ok_or_else(|| {
            NaviError::NotFound(format!(
                "no existing ancestor for '{}' inside the workspace",
                candidate.display()
            ))
        })?;
        let canonical = std::fs::canonicalize(&ancestor)?;
        if !is_within(&self.root, &canonical) {
            return Err(NaviError::PermissionDenied(format!(
                "path '{}' escapes the workspace through a symlink",
                candidate.display()
            )));
        }
        let suffix = candidate
            .strip_prefix(&ancestor)
            .map_err(|_| NaviError::Internal("resolved ancestor is not a path prefix".into()))?;
        Ok(canonical.join(suffix))
    }
}

/// Component-wise containment: `/work-extra` is not inside `/work`.
fn is_within(root: &Path, candidate: &Path) -> bool {
    candidate.strip_prefix(root).is_ok()
}

/// Rejects traversal before any filesystem operation is attempted.
fn reject_traversal(path: &Path) -> NaviResult<()> {
    for component in path.components() {
        match component {
            Component::ParentDir | Component::Prefix(_) => {
                return Err(NaviError::InvalidInput(format!(
                    "path traversal is not allowed: '{}'",
                    path.display()
                )));
            }
            Component::RootDir | Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Finds the deepest component that exists, counting a dangling symlink as
/// existing so canonicalization reports it instead of silently skipping it.
fn deepest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cursor = path;
    loop {
        if std::fs::symlink_metadata(cursor).is_ok() {
            return Some(cursor.to_path_buf());
        }
        cursor = cursor.parent()?;
    }
}

/// Reads the effective UID from procfs without native calls; returns `None` if unavailable.
#[cfg(unix)]
fn current_effective_uid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let uid_line = status.lines().find(|line| line.starts_with("Uid:"))?;
        uid_line.split_whitespace().nth(2)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "navi-fs-guard-{label}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create temp root");
        path
    }

    #[test]
    fn new_accepts_directory_and_rejects_missing_or_file_root() {
        let root = temp_root("new");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        assert!(guard.root.is_dir());

        let file = root.join("plain.txt");
        std::fs::write(&file, "x").expect("write file");
        assert!(WorkspaceFsGuard::new(file).is_err());
        assert!(WorkspaceFsGuard::new(root.join("missing")).is_err());
        assert!(WorkspaceFsGuard::new(PathBuf::new()).is_err());

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn resolves_relative_and_absolute_paths_inside_root() {
        let root = temp_root("resolve");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        let file = root.join("note.txt");
        std::fs::write(&file, "hello").expect("write file");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");
        let canonical_file = canonical_root.join("note.txt");

        assert_eq!(
            guard.resolve_read(Path::new("note.txt")).expect("relative"),
            canonical_file
        );
        assert_eq!(
            guard.resolve_read(Path::new("./note.txt")).expect("curdir"),
            canonical_file
        );
        assert_eq!(guard.resolve_read(&file).expect("absolute"), canonical_file);

        let pending = guard
            .resolve_write(Path::new("nested/new.txt"))
            .expect("write target");
        assert_eq!(pending, canonical_root.join("nested/new.txt"));

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn empty_and_current_directory_paths_resolve_to_root() {
        let root = temp_root("empty");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");

        assert_eq!(
            guard.resolve_read(Path::new("")).expect("empty path"),
            canonical_root
        );
        assert_eq!(
            guard.resolve_read(Path::new(".")).expect("current dir"),
            canonical_root
        );
        assert!(guard.check_no_symlink_escape(Path::new(".")).is_ok());

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let root = temp_root("traversal");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        assert!(guard.resolve_read(Path::new("../outside.txt")).is_err());
        assert!(
            guard
                .resolve_write(Path::new("a/../../outside.txt"))
                .is_err()
        );
        assert!(
            guard
                .resolve_read(&root.join("..").join("outside.txt"))
                .is_err()
        );
        assert!(guard.check_no_symlink_escape(Path::new("a/../b")).is_err());

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn rejects_absolute_paths_outside_and_sibling_prefixes() {
        let root = temp_root("absolute");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        let sibling = PathBuf::from(format!("{}-extra", root.display()));
        std::fs::create_dir(&sibling).expect("create sibling");
        let secret = sibling.join("secret.txt");
        std::fs::write(&secret, "secret").expect("write secret");

        assert!(guard.resolve_read(&secret).is_err());
        assert!(guard.resolve_write(&secret).is_err());
        assert!(guard.check_no_symlink_escape(&secret).is_err());
        assert!(guard.resolve_read(Path::new("/etc/passwd")).is_err());

        std::fs::remove_dir_all(&root).expect("cleanup root");
        std::fs::remove_dir_all(&sibling).expect("cleanup sibling");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_pointing_outside_root() {
        let root = temp_root("symlink-out");
        let outside = temp_root("symlink-outside");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");

        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "secret").expect("write outside");
        let link = root.join("escape.txt");
        std::os::unix::fs::symlink(&outside_file, &link).expect("symlink file");
        assert!(guard.resolve_read(&link).is_err());
        assert!(guard.resolve_write(&link).is_err());
        assert!(guard.check_no_symlink_escape(&link).is_err());

        let outside_dir = outside.join("dir");
        std::fs::create_dir(&outside_dir).expect("create outside dir");
        let dir_link = root.join("dir-link");
        std::os::unix::fs::symlink(&outside_dir, &dir_link).expect("symlink dir");
        assert!(guard.resolve_read(&dir_link.join("child.txt")).is_err());
        assert!(guard.resolve_write(&dir_link.join("new.txt")).is_err());

        std::fs::remove_dir_all(&root).expect("cleanup root");
        std::fs::remove_dir_all(&outside).expect("cleanup outside");
    }

    #[cfg(unix)]
    #[test]
    fn allows_symlinks_that_stay_inside_root() {
        let root = temp_root("symlink-in");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        let target = root.join("target.txt");
        std::fs::write(&target, "data").expect("write target");
        let link = root.join("alias.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let canonical_target = std::fs::canonicalize(&target).expect("canonical target");
        assert_eq!(
            guard.resolve_read(&link).expect("inside symlink"),
            canonical_target
        );
        assert_eq!(
            guard.resolve_write(&link).expect("inside symlink write"),
            canonical_target
        );
        assert!(guard.check_no_symlink_escape(&link).is_ok());

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn check_no_symlink_escape_accepts_root_and_confined_files() {
        let root = temp_root("check");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");
        assert!(guard.check_no_symlink_escape(&canonical_root).is_ok());

        let file = canonical_root.join("file.txt");
        std::fs::write(&file, "data").expect("write file");
        assert!(guard.check_no_symlink_escape(&file).is_ok());
        assert!(
            guard
                .check_no_symlink_escape(&canonical_root.join("future.txt"))
                .is_ok()
        );

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_mode_checks_permission_bits() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("mode");
        let secure = root.join("secure.txt");
        std::fs::write(&secure, "x").expect("write file");
        std::fs::set_permissions(&secure, std::fs::Permissions::from_mode(0o600))
            .expect("set private mode");
        assert!(WorkspaceFsGuard::verify_owner_only_mode(&secure).is_ok());

        let loose = root.join("loose.txt");
        std::fs::write(&loose, "x").expect("write file");
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644))
            .expect("set group-readable mode");
        assert!(WorkspaceFsGuard::verify_owner_only_mode(&loose).is_err());

        let dir = root.join("private");
        std::fs::create_dir(&dir).expect("create dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("set private dir mode");
        assert!(WorkspaceFsGuard::verify_owner_only_mode(&dir).is_ok());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o705))
            .expect("set loose dir mode");
        assert!(WorkspaceFsGuard::verify_owner_only_mode(&dir).is_err());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore dir mode");
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_mode_rejects_symlink_and_missing_path() {
        let root = temp_root("mode-symlink");
        let target = root.join("target.txt");
        std::fs::write(&target, "x").expect("write target");
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        assert!(WorkspaceFsGuard::verify_owner_only_mode(&link).is_err());
        assert!(WorkspaceFsGuard::verify_owner_only_mode(&root.join("missing.txt")).is_err());

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn resolve_write_returns_pending_target_without_creating_parents() {
        let root = temp_root("write-pending");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");

        let target = guard
            .resolve_write(Path::new("a/b/c.txt"))
            .expect("pending write target");
        assert_eq!(target, canonical_root.join("a/b/c.txt"));
        assert!(!target.exists(), "resolution must not create the target");
        assert!(
            !canonical_root.join("a").exists(),
            "resolution must not create parent directories"
        );

        let read_target = guard
            .resolve_read(Path::new("missing.txt"))
            .expect("read resolution does not require existence");
        assert_eq!(read_target, canonical_root.join("missing.txt"));

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlinks_inside_and_outside_root() {
        let root = temp_root("dangling");
        let guard = WorkspaceFsGuard::new(root.clone()).expect("valid root");

        let inside_link = root.join("dangling-inside.txt");
        std::os::unix::fs::symlink(Path::new("missing.txt"), &inside_link)
            .expect("symlink inside root");
        assert!(guard.resolve_read(&inside_link).is_err());
        assert!(guard.resolve_write(&inside_link).is_err());
        assert!(guard.check_no_symlink_escape(&inside_link).is_err());

        let outside_link = root.join("dangling-outside.txt");
        std::os::unix::fs::symlink(Path::new("../missing.txt"), &outside_link)
            .expect("symlink outside root");
        assert!(guard.resolve_read(&outside_link).is_err());
        assert!(guard.resolve_write(&outside_link).is_err());
        assert!(guard.check_no_symlink_escape(&outside_link).is_err());

        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn new_canonicalizes_a_symlinked_root() {
        let real = temp_root("real-root");
        let alias_parent = temp_root("alias-parent");
        let alias = alias_parent.join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink root");

        let guard = WorkspaceFsGuard::new(alias.clone()).expect("symlinked root");
        let canonical_real = std::fs::canonicalize(&real).expect("canonical real root");
        assert_eq!(guard.root, canonical_real);

        std::fs::write(real.join("file.txt"), "data").expect("write file");
        assert_eq!(
            guard.resolve_read(Path::new("file.txt")).expect("relative"),
            canonical_real.join("file.txt")
        );
        // Absolute paths are compared against the canonical root, not the alias.
        assert!(guard.resolve_read(&alias.join("file.txt")).is_err());

        std::fs::remove_dir_all(&real).expect("cleanup real root");
        std::fs::remove_dir_all(&alias_parent).expect("cleanup alias parent");
    }
}
