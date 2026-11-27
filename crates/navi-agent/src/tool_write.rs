//! Built-in `write` tool: writes UTF-8 content to a workspace file atomically.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Maximum number of temporary file names tried before reporting a failure.
const MAX_TEMP_ATTEMPTS: u64 = 8;

/// Arguments of the built-in `write` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteToolArgs {
    /// File path, relative to the workspace root, or absolute inside it.
    pub path: String,
    /// UTF-8 content written to the file.
    pub content: String,
}

/// Result of a `write` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteToolOutput {
    /// Path supplied by the caller.
    pub path: String,
    /// Number of UTF-8 bytes written.
    pub bytes_written: u64,
    /// Whether the file did not exist before the write.
    pub created: bool,
}

/// Writes `args.content` to the workspace file named by `args.path`.
///
/// Relative paths are resolved against `workspace_root`; absolute paths are accepted only when
/// they stay inside it. Missing parent directories are created, symlinks that resolve outside the
/// workspace are rejected, and the file is replaced by a rename from a temporary file in the same
/// directory so readers never observe a partially written file.
pub fn execute_write(workspace_root: &Path, args: &WriteToolArgs) -> NaviResult<WriteToolOutput> {
    let resolved = resolve_workspace_path(workspace_root, &args.path)?;

    let created = match fs::symlink_metadata(&resolved) {
        Ok(_) => false,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(write_error(&err, &resolved)),
    };

    let parent = resolved.parent().ok_or_else(|| {
        NaviError::InvalidInput(format!("path '{}' has no parent", resolved.display()))
    })?;
    fs::create_dir_all(parent).map_err(|err| write_error(&err, parent))?;
    write_atomically(&resolved, args.content.as_bytes())?;

    Ok(WriteToolOutput {
        path: args.path.clone(),
        bytes_written: args.content.len() as u64,
        created,
    })
}

/// Resolves a caller-supplied path and rejects anything that escapes `workspace_root`.
fn resolve_workspace_path(workspace_root: &Path, user_path: &str) -> NaviResult<PathBuf> {
    if user_path.is_empty() {
        return Err(NaviError::InvalidInput(
            "write path must not be empty".to_owned(),
        ));
    }
    if user_path.as_bytes().contains(&0) {
        return Err(NaviError::InvalidInput(format!(
            "write path contains a NUL byte: {user_path:?}"
        )));
    }

    let root_abs = absolute_path(workspace_root)?;
    let canonical_root = fs::canonicalize(&root_abs).map_err(|err| write_error(&err, &root_abs))?;
    if !canonical_root.is_dir() {
        return Err(NaviError::InvalidInput(format!(
            "workspace root '{}' is not a directory",
            root_abs.display()
        )));
    }

    let requested = Path::new(user_path);
    let candidate = if requested.is_absolute() {
        normalize_absolute(requested)?
    } else {
        root_abs.join(normalize_relative(requested)?)
    };

    // Comparing both spellings lets a caller use either the workspace path or its canonical path
    // when the workspace root itself is reached through a symlink.
    if !candidate.starts_with(&root_abs) && !candidate.starts_with(&canonical_root) {
        return Err(path_escape_error(user_path, workspace_root));
    }
    if escapes_via_symlink(&canonical_root, &candidate) {
        return Err(path_escape_error(user_path, workspace_root));
    }

    Ok(candidate)
}

/// Reports whether a symlink on the path to `candidate` resolves outside the canonical root.
fn escapes_via_symlink(canonical_root: &Path, candidate: &Path) -> bool {
    let existing = deepest_existing_ancestor(candidate);
    // If even the deepest existing ancestor cannot be resolved, refuse the write rather than
    // guessing about its real location.
    fs::canonicalize(&existing).map_or(true, |resolved| !resolved.starts_with(canonical_root))
}

/// Finds the longest prefix of `path` that currently exists on the filesystem.
fn deepest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if fs::symlink_metadata(current).is_ok() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return path.to_path_buf(),
        }
    }
}

/// Makes `path` absolute by resolving it against the current directory and collapsing `..`.
fn absolute_path(path: &Path) -> NaviResult<PathBuf> {
    if path.is_absolute() {
        normalize_absolute(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|err| NaviError::Io(format!("cannot read current directory: {err}")))?;
        normalize_absolute(&cwd.join(path))
    }
}

/// Collapses `.` and `..` in a relative path, rejecting attempts to leave the workspace.
fn normalize_relative(path: &Path) -> NaviResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir if normalized.pop() => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(NaviError::InvalidInput(format!(
                    "path '{}' escapes the workspace root",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

/// Collapses `.` and `..` in an absolute path without touching the filesystem.
fn normalize_absolute(path: &Path) -> NaviResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir if normalized.pop() => {}
            Component::ParentDir => {
                return Err(NaviError::InvalidInput(format!(
                    "path '{}' escapes the filesystem root",
                    path.display()
                )));
            }
        }
    }
    Ok(normalized)
}

/// Writes to a private file in `target`'s directory, then renames it over `target`.
fn write_atomically(target: &Path, bytes: &[u8]) -> NaviResult<()> {
    let parent = target.parent().ok_or_else(|| {
        NaviError::InvalidInput(format!("path '{}' has no parent", target.display()))
    })?;

    for _ in 0..MAX_TEMP_ATTEMPTS {
        let temp = temp_path(parent);
        let mut file = match File::options().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(write_error(&err, &temp)),
        };

        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        // Windows refuses to rename an open file, so close it before the rename.
        drop(file);
        if let Err(err) = written {
            let _ = fs::remove_file(&temp);
            return Err(write_error(&err, &temp));
        }
        if let Err(err) = fs::rename(&temp, target) {
            let _ = fs::remove_file(&temp);
            return Err(write_error(&err, target));
        }
        return Ok(());
    }

    Err(NaviError::Io(format!(
        "could not reserve a temporary file in '{}'",
        parent.display()
    )))
}

/// Builds a collision-resistant temporary file name inside `parent`.
fn temp_path(parent: &Path) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    parent.join(format!(
        ".navi-write-{}-{nanos}-{id}.tmp",
        std::process::id()
    ))
}

/// Builds the error used for every workspace containment failure.
fn path_escape_error(user_path: &str, workspace_root: &Path) -> NaviError {
    NaviError::InvalidInput(format!(
        "path '{user_path}' escapes workspace root '{}'",
        workspace_root.display()
    ))
}

/// Maps an I/O failure to the corresponding shared error kind.
fn write_error(err: &io::Error, path: &Path) -> NaviError {
    let message = format!("{}: {err}", path.display());
    match err.kind() {
        io::ErrorKind::NotFound => NaviError::NotFound(message),
        io::ErrorKind::PermissionDenied => NaviError::PermissionDenied(message),
        _ => NaviError::Io(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A unique workspace directory that is removed when the test ends.
    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("navi-tool-write-{}-{id}", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).expect("remove stale test directory");
            }
            fs::create_dir_all(&root).expect("create test directory");
            Self { root }
        }

        fn args(&self, path: &str, content: &str) -> WriteToolArgs {
            WriteToolArgs {
                path: path.to_owned(),
                content: content.to_owned(),
            }
        }

        fn names(&self) -> Vec<String> {
            let mut names = fs::read_dir(&self.root)
                .expect("list workspace")
                .map(|entry| {
                    entry
                        .expect("directory entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn writes_new_file_and_reports_metadata() {
        let workspace = TempWorkspace::new();

        let output = execute_write(&workspace.root, &workspace.args("note.txt", "hello"))
            .expect("write new file");

        assert_eq!(output.path, "note.txt");
        assert_eq!(output.bytes_written, 5);
        assert!(output.created);
        assert_eq!(
            fs::read_to_string(workspace.root.join("note.txt")).expect("read written file"),
            "hello"
        );
    }

    #[test]
    fn overwrites_existing_file_without_reporting_created() {
        let workspace = TempWorkspace::new();
        let file = workspace.root.join("note.txt");
        fs::write(&file, b"a much longer original content").expect("seed file");

        let output = execute_write(&workspace.root, &workspace.args("note.txt", "new"))
            .expect("overwrite file");

        assert!(!output.created);
        assert_eq!(output.bytes_written, 3);
        assert_eq!(fs::read_to_string(&file).expect("read written file"), "new");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let workspace = TempWorkspace::new();

        let output = execute_write(&workspace.root, &workspace.args("a/b/c/deep.txt", "deep"))
            .expect("write nested file");

        assert!(output.created);
        assert!(workspace.root.join("a/b/c").is_dir());
        assert_eq!(
            fs::read_to_string(workspace.root.join("a/b/c/deep.txt")).expect("read nested file"),
            "deep"
        );
    }

    #[test]
    fn accepts_absolute_path_inside_workspace() {
        let workspace = TempWorkspace::new();
        let absolute = workspace
            .root
            .join("absolute.txt")
            .to_string_lossy()
            .into_owned();

        let output = execute_write(&workspace.root, &workspace.args(&absolute, "ok"))
            .expect("write absolute path inside workspace");

        assert!(output.created);
        assert_eq!(output.path, absolute);
        assert_eq!(
            fs::read_to_string(workspace.root.join("absolute.txt")).expect("read file"),
            "ok"
        );
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let workspace = TempWorkspace::new();
        let name = workspace
            .root
            .file_name()
            .expect("workspace name")
            .to_string_lossy();
        let marker = format!("{name}-outside.txt");
        let escape = format!("../{marker}");

        for path in [escape.as_str(), "a/../../outside.txt", "/etc/passwd"] {
            let error = execute_write(&workspace.root, &workspace.args(path, "secret"))
                .expect_err("paths outside the workspace must be rejected");
            assert!(
                matches!(error, NaviError::InvalidInput(_)),
                "{path}: {error}"
            );
        }
        let outside = workspace
            .root
            .parent()
            .expect("workspace parent")
            .join(marker);
        assert!(!outside.exists());
    }

    #[test]
    fn rejects_absolute_path_outside_workspace() {
        let workspace = TempWorkspace::new();
        let outside = workspace.root.with_extension("outside");
        let path = outside.join("file.txt").to_string_lossy().into_owned();

        let error = execute_write(&workspace.root, &workspace.args(&path, "secret"))
            .expect_err("absolute path outside the workspace must be rejected");

        assert!(matches!(error, NaviError::InvalidInput(_)));
        assert!(!path_exists(&path));
    }

    #[test]
    fn rejects_empty_path_and_nul_byte() {
        let workspace = TempWorkspace::new();

        for path in ["", "bad\0path"] {
            let error = execute_write(&workspace.root, &workspace.args(path, "x"))
                .expect_err("invalid paths must be rejected");
            assert!(
                matches!(error, NaviError::InvalidInput(_)),
                "{path:?}: {error}"
            );
        }
    }

    #[test]
    fn writes_empty_content() {
        let workspace = TempWorkspace::new();

        let output = execute_write(&workspace.root, &workspace.args("empty.txt", ""))
            .expect("write empty file");

        assert!(output.created);
        assert_eq!(output.bytes_written, 0);
        assert_eq!(
            fs::read(workspace.root.join("empty.txt")).expect("read file"),
            b""
        );
    }

    #[test]
    fn counts_utf8_bytes_not_characters() {
        let workspace = TempWorkspace::new();
        let content = "héllo 🌍";

        let output = execute_write(&workspace.root, &workspace.args("unicode.txt", content))
            .expect("write unicode content");

        assert_eq!(output.bytes_written, content.len() as u64);
        assert_ne!(output.bytes_written, content.chars().count() as u64);
        assert_eq!(
            fs::read_to_string(workspace.root.join("unicode.txt")).expect("read file"),
            content
        );
    }

    #[test]
    fn normalizes_dot_and_redundant_components() {
        let workspace = TempWorkspace::new();

        let output = execute_write(
            &workspace.root,
            &workspace.args("./nested/../flat.txt", "flat"),
        )
        .expect("write normalized path");

        assert_eq!(output.path, "./nested/../flat.txt");
        assert_eq!(
            fs::read_to_string(workspace.root.join("flat.txt")).expect("read file"),
            "flat"
        );
        assert!(!workspace.root.join("nested").exists());
    }

    #[test]
    fn leaves_no_temporary_files_after_success() {
        let workspace = TempWorkspace::new();

        execute_write(&workspace.root, &workspace.args("ok.txt", "content")).expect("write file");

        assert_eq!(workspace.names(), vec!["ok.txt".to_owned()]);
    }

    #[test]
    fn cleans_up_temporary_file_when_rename_fails() {
        let workspace = TempWorkspace::new();
        fs::create_dir(workspace.root.join("target.txt")).expect("create directory target");

        let error = execute_write(&workspace.root, &workspace.args("target.txt", "x"))
            .expect_err("renaming over a directory must fail");

        assert!(matches!(error, NaviError::Io(_)));
        assert_eq!(workspace.names(), vec!["target.txt".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_parent_outside_workspace() {
        let workspace = TempWorkspace::new();
        let outside = workspace.root.with_extension("outside");
        fs::create_dir_all(&outside).expect("create directory outside workspace");
        std::os::unix::fs::symlink(&outside, workspace.root.join("escape"))
            .expect("create symlink to outside directory");

        let error = execute_write(
            &workspace.root,
            &workspace.args("escape/file.txt", "secret"),
        )
        .expect_err("symlink escape must be rejected");

        assert!(matches!(error, NaviError::InvalidInput(_)));
        assert!(!outside.join("file.txt").exists());
        fs::remove_dir_all(&outside).expect("remove outside directory");
    }

    /// Checks existence without following symlinks, for paths supplied as strings.
    fn path_exists(path: &str) -> bool {
        fs::symlink_metadata(Path::new(path)).is_ok()
    }
}
