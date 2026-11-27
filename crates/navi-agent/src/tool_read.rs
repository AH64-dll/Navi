//! Built-in `read` tool: reads a text file inside the workspace with line and byte caps.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Default cap on the number of lines shown by a read.
const DEFAULT_MAX_LINES: usize = 2000;
/// Default cap on the number of UTF-8 bytes shown by a read (50 KiB).
const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Arguments of the built-in `read` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadToolArgs {
    /// File path, relative to the workspace root, or an absolute path inside it.
    pub path: String,
    /// First line to show, 1-indexed. Defaults to the first line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Maximum number of lines requested by the caller, before the built-in caps apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Result of a `read` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadToolOutput {
    /// The shown text: complete lines joined by `\n`, with no continuation notices.
    pub content: String,
    /// Number of lines in the whole file.
    pub total_lines: usize,
    /// Number of complete lines included in `content`.
    pub shown_lines: usize,
    /// Whether the shown range stops before the end of the file.
    pub truncated: bool,
}

/// Reads a text file from the workspace, applying line and byte limits.
///
/// Paths that escape the workspace (lexically or through a symlink) and files containing a NUL
/// byte are rejected. Only complete lines are returned; the caps never cut a line in half.
pub fn execute_read(workspace_root: &Path, args: &ReadToolArgs) -> NaviResult<ReadToolOutput> {
    let resolved = resolve_workspace_path(workspace_root, &args.path)?;

    let bytes = fs::read(&resolved).map_err(|err| read_error(&err, &resolved))?;
    if bytes.contains(&0) {
        return Err(NaviError::InvalidInput(format!(
            "binary file cannot be read as text: {}",
            resolved.display()
        )));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines = split_lines(text.as_ref());
    let total_lines = lines.len();

    let requested_offset = args.offset.unwrap_or(1);
    let start = requested_offset.saturating_sub(1);
    // An empty file at offset 1 has a valid, empty range; any other offset past the end is an error.
    if start > total_lines || (start == total_lines && requested_offset > 1) {
        return Err(NaviError::InvalidInput(format!(
            "offset {requested_offset} is beyond end of file ({total_lines} lines total)"
        )));
    }

    let line_cap = args
        .limit
        .unwrap_or(DEFAULT_MAX_LINES)
        .min(DEFAULT_MAX_LINES);
    let mut shown: Vec<&str> = Vec::new();
    let mut shown_bytes = 0usize;
    for line in lines.iter().skip(start).take(line_cap) {
        // Counting the separator between lines keeps the byte cap equivalent to truncating the
        // joined output, without ever emitting a partial line.
        let cost = line.len() + usize::from(!shown.is_empty());
        let next_bytes = shown_bytes.saturating_add(cost);
        if next_bytes > DEFAULT_MAX_BYTES {
            break;
        }
        shown_bytes = next_bytes;
        shown.push(line);
    }

    Ok(ReadToolOutput {
        content: shown.join("\n"),
        total_lines,
        shown_lines: shown.len(),
        truncated: start.saturating_add(shown.len()) < total_lines,
    })
}

/// Resolves a user-supplied path, rejecting any path that leaves `workspace_root`.
fn resolve_workspace_path(workspace_root: &Path, user_path: &str) -> NaviResult<PathBuf> {
    if user_path.is_empty() {
        return Err(NaviError::InvalidInput(
            "read path must not be empty".to_owned(),
        ));
    }
    let requested = Path::new(user_path);
    let candidate = if requested.is_absolute() {
        let normalized = normalize_absolute(requested)?;
        let root = normalize_absolute(workspace_root)?;
        if !normalized.starts_with(&root) {
            return Err(path_escape_error(user_path, workspace_root));
        }
        normalized
    } else {
        let mut relative = PathBuf::new();
        for component in requested.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => relative.push(part),
                Component::ParentDir if relative.pop() => {}
                Component::ParentDir => return Err(path_escape_error(user_path, workspace_root)),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(path_escape_error(user_path, workspace_root));
                }
            }
        }
        workspace_root.join(relative)
    };

    // A symlink inside the workspace may still point outside it. Check the real location when both
    // paths exist; a missing file is reported by `fs::read` below.
    if escapes_via_symlink(workspace_root, &candidate) {
        return Err(path_escape_error(user_path, workspace_root));
    }

    Ok(candidate)
}

/// Reports whether an existing symlink resolves outside the canonical workspace root.
fn escapes_via_symlink(workspace_root: &Path, candidate: &Path) -> bool {
    let Ok(root) = fs::canonicalize(workspace_root) else {
        return false;
    };
    let Ok(resolved) = fs::canonicalize(candidate) else {
        return false;
    };
    !resolved.starts_with(root)
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

/// Builds the error used for every workspace containment failure.
fn path_escape_error(user_path: &str, workspace_root: &Path) -> NaviError {
    NaviError::InvalidInput(format!(
        "path '{user_path}' escapes workspace root '{}'",
        workspace_root.display()
    ))
}

/// Maps an I/O failure to the corresponding shared error kind.
fn read_error(err: &io::Error, path: &Path) -> NaviError {
    let message = format!("{}: {err}", path.display());
    match err.kind() {
        io::ErrorKind::NotFound => NaviError::NotFound(message),
        io::ErrorKind::PermissionDenied => NaviError::PermissionDenied(message),
        _ => NaviError::Io(message),
    }
}

/// Splits text on `\n`, treating a trailing newline as a terminator rather than an empty line.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("navi-tool-read-{}-{id}", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).expect("remove stale test directory");
            }
            fs::create_dir_all(&root).expect("create test directory");
            Self { root }
        }

        fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(&path, contents).expect("write test file");
            path
        }

        fn args(&self, path: &str, offset: Option<usize>, limit: Option<usize>) -> ReadToolArgs {
            ReadToolArgs {
                path: path.to_owned(),
                offset,
                limit,
            }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn reads_whole_text_file() {
        let workspace = TempWorkspace::new();
        workspace.write("note.txt", b"alpha\nbeta\ngamma");

        let output = execute_read(&workspace.root, &workspace.args("note.txt", None, None))
            .expect("read text file");

        assert_eq!(output.content, "alpha\nbeta\ngamma");
        assert_eq!(output.total_lines, 3);
        assert_eq!(output.shown_lines, 3);
        assert!(!output.truncated);
    }

    #[test]
    fn offset_is_one_indexed_and_combines_with_limit() {
        let workspace = TempWorkspace::new();
        workspace.write("lines.txt", b"one\ntwo\nthree\nfour");

        let output = execute_read(
            &workspace.root,
            &workspace.args("lines.txt", Some(2), Some(2)),
        )
        .expect("read line range");

        assert_eq!(output.content, "two\nthree");
        assert_eq!(output.total_lines, 4);
        assert_eq!(output.shown_lines, 2);
        assert!(output.truncated);
    }

    #[test]
    fn limit_stops_at_requested_line_count() {
        let workspace = TempWorkspace::new();
        workspace.write("many.txt", b"a\nb\nc\nd\ne");

        let output = execute_read(&workspace.root, &workspace.args("many.txt", None, Some(2)))
            .expect("read limited lines");

        assert_eq!(output.content, "a\nb");
        assert_eq!(output.total_lines, 5);
        assert_eq!(output.shown_lines, 2);
        assert!(output.truncated);
    }

    #[test]
    fn truncates_at_default_line_cap() {
        let workspace = TempWorkspace::new();
        let contents = (1..=2001)
            .map(|number| format!("line-{number}"))
            .collect::<Vec<_>>()
            .join("\n");
        workspace.write("long.txt", contents.as_bytes());

        let output = execute_read(&workspace.root, &workspace.args("long.txt", None, None))
            .expect("read file over the line cap");

        assert_eq!(output.total_lines, 2001);
        assert_eq!(output.shown_lines, DEFAULT_MAX_LINES);
        assert!(output.content.starts_with("line-1\nline-2\n"));
        assert!(output.content.ends_with("line-2000"));
        assert!(output.truncated);
    }

    #[test]
    fn byte_cap_never_returns_a_partial_line() {
        let workspace = TempWorkspace::new();
        let mut contents = String::from("head\n");
        contents.push_str(&"x".repeat(DEFAULT_MAX_BYTES));
        contents.push_str("\ntail");
        workspace.write("wide.txt", contents.as_bytes());

        let output = execute_read(&workspace.root, &workspace.args("wide.txt", None, None))
            .expect("read file over the byte cap");

        assert_eq!(output.content, "head");
        assert_eq!(output.shown_lines, 1);
        assert_eq!(output.total_lines, 3);
        assert!(output.truncated);
    }

    #[test]
    fn rejects_binary_files_containing_nul_bytes() {
        let workspace = TempWorkspace::new();
        workspace.write("binary.bin", b"text\x00more");

        let error = execute_read(&workspace.root, &workspace.args("binary.bin", None, None))
            .expect_err("binary files must be rejected");

        assert!(matches!(&error, NaviError::InvalidInput(_)));
        assert!(error.to_string().contains("binary"));
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let workspace = TempWorkspace::new();
        workspace.write("nested/inside.txt", b"inside");

        for path in ["../outside.txt", "nested/../../outside.txt", "/etc/passwd"] {
            let error = execute_read(&workspace.root, &workspace.args(path, None, None))
                .expect_err("paths outside the workspace must be rejected");
            assert!(
                matches!(&error, NaviError::InvalidInput(_)),
                "{path}: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside_workspace() {
        let workspace = TempWorkspace::new();
        let outside = workspace.root.with_extension("outside");
        fs::write(&outside, b"secret").expect("write outside file");
        let link = workspace.root.join("link.txt");
        symlink_to(&outside, &link);

        let error = execute_read(&workspace.root, &workspace.args("link.txt", None, None))
            .expect_err("symlink escape must be rejected");

        assert!(matches!(&error, NaviError::InvalidInput(_)));
        fs::remove_file(&outside).expect("remove outside file");
    }

    #[test]
    fn accepts_absolute_paths_inside_workspace() {
        let workspace = TempWorkspace::new();
        let file = workspace.write("absolute.txt", b"ok");
        let path = file.to_string_lossy().into_owned();

        let output = execute_read(&workspace.root, &workspace.args(&path, None, None))
            .expect("read absolute path inside workspace");

        assert_eq!(output.content, "ok");
        assert_eq!(output.total_lines, 1);
    }

    #[test]
    fn handles_empty_file_and_out_of_range_offset() {
        let workspace = TempWorkspace::new();
        workspace.write("empty.txt", b"");
        workspace.write("short.txt", b"only");

        let empty = execute_read(&workspace.root, &workspace.args("empty.txt", None, None))
            .expect("read empty file");
        assert_eq!(empty.content, "");
        assert_eq!(empty.total_lines, 0);
        assert_eq!(empty.shown_lines, 0);
        assert!(!empty.truncated);

        let error = execute_read(&workspace.root, &workspace.args("short.txt", Some(5), None))
            .expect_err("offset past the end must be rejected");
        assert!(matches!(&error, NaviError::InvalidInput(_)));
    }

    #[test]
    fn missing_file_maps_to_not_found() {
        let workspace = TempWorkspace::new();

        let error = execute_read(&workspace.root, &workspace.args("missing.txt", None, None))
            .expect_err("missing file must be reported");

        assert!(matches!(&error, NaviError::NotFound(_)));
    }

    #[test]
    fn trailing_newline_does_not_add_an_empty_line() {
        let workspace = TempWorkspace::new();
        workspace.write("trailing.txt", b"first\nsecond\n");

        let output = execute_read(&workspace.root, &workspace.args("trailing.txt", None, None))
            .expect("read file with trailing newline");

        assert_eq!(output.content, "first\nsecond");
        assert_eq!(output.total_lines, 2);
        assert_eq!(output.shown_lines, 2);
        assert!(!output.truncated);
    }

    #[cfg(unix)]
    fn symlink_to(target: &Path, link: &Path) {
        use std::os::unix::fs::symlink;
        symlink(target, link).expect("create symlink");
    }
}
