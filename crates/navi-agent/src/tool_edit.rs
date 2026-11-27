//! Built-in `edit` tool: exact-text replacement in a workspace file with a unified diff.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};

/// Largest LCS table (in cells) built for a line diff. Larger inputs use a single-hunk diff.
const LCS_CELL_LIMIT: usize = 4_000_000;
/// Number of unchanged lines kept around each changed hunk of the unified diff.
const DIFF_CONTEXT_LINES: usize = 3;

/// Arguments of the built-in `edit` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditToolArgs {
    /// File path, relative to the workspace root, or an absolute path inside it.
    pub path: String,
    /// Exact text to replace; it must occur exactly once in the file.
    #[serde(alias = "oldText")]
    pub old_text: String,
    /// Text written in place of `old_text`.
    #[serde(alias = "newText")]
    pub new_text: String,
}

/// Result of an `edit` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditToolOutput {
    /// Path supplied by the caller.
    pub path: String,
    /// Number of replacements applied to the file.
    pub replacements: usize,
    /// Unified diff of the change, with the caller-supplied path as both labels.
    pub unified_diff: String,
}

/// Replaces the unique exact match of `args.old_text` in the workspace file and returns its diff.
///
/// The path is resolved against `workspace_root`; both lexical escapes and symlinks out of the
/// workspace are rejected. Matching happens on an LF-normalized view of the file, so callers do
/// not need to include CRLF bytes in `old_text`. The text written back keeps the original line
/// endings of untouched lines, and inserted lines adopt the ending of the replaced region. A BOM
/// is preserved but excluded from matching. The diff is computed before the write so it cannot
/// fail after the file has changed.
pub fn execute_edit(workspace_root: &Path, args: &EditToolArgs) -> NaviResult<EditToolOutput> {
    let resolved = resolve_workspace_path(workspace_root, &args.path)?;

    let bytes = fs::read(&resolved).map_err(|err| edit_error(&err, &resolved))?;
    if bytes.contains(&0) {
        return Err(NaviError::InvalidInput(format!(
            "binary file cannot be edited as text: {}",
            resolved.display()
        )));
    }
    let raw = String::from_utf8(bytes).map_err(|_| {
        NaviError::InvalidInput(format!("file is not valid UTF-8: {}", resolved.display()))
    })?;
    let (bom, content) = match raw.strip_prefix('\u{feff}') {
        Some(rest) => (&raw[..raw.len() - rest.len()], rest),
        None => ("", raw.as_str()),
    };

    let old_text = normalize_to_lf(&args.old_text);
    let new_text = normalize_to_lf(&args.new_text);
    if old_text.is_empty() {
        return Err(NaviError::InvalidInput(format!(
            "old_text must not be empty when editing {}",
            args.path
        )));
    }

    let normalized = normalize_to_lf(content);
    let occurrences = normalized.matches(old_text.as_str()).count();
    if occurrences == 0 {
        return Err(NaviError::InvalidInput(format!(
            "could not find old_text in {}: it must match exactly, including whitespace and newlines",
            args.path
        )));
    }
    if occurrences > 1 {
        return Err(NaviError::InvalidInput(format!(
            "old_text is not unique in {} (found {occurrences} occurrences); provide more context to make it unique",
            args.path
        )));
    }
    let match_index = normalized.find(old_text.as_str()).ok_or_else(|| {
        NaviError::InvalidInput(format!("could not locate old_text in {}", args.path))
    })?;

    let mut new_normalized = String::with_capacity(normalized.len().saturating_add(new_text.len()));
    new_normalized.push_str(&normalized[..match_index]);
    new_normalized.push_str(&new_text);
    new_normalized.push_str(&normalized[match_index + old_text.len()..]);
    if new_normalized == normalized {
        return Err(NaviError::InvalidInput(format!(
            "no changes made to {}: the replacement produced identical content",
            args.path
        )));
    }

    let unified_diff = compute_unified_diff(&normalized, &new_normalized, &args.path);

    let map = normalized_offset_map(content);
    let original_start = map.get(match_index).copied().ok_or_else(|| {
        NaviError::Internal("normalized edit start is outside the original file".to_owned())
    })?;
    let original_end = map
        .get(match_index + old_text.len())
        .copied()
        .ok_or_else(|| {
            NaviError::Internal("normalized edit end is outside the original file".to_owned())
        })?;
    let prefix = content.get(..original_start).ok_or_else(|| {
        NaviError::Internal(
            "edit start is not a character boundary in the original file".to_owned(),
        )
    })?;
    let suffix = content.get(original_end..).ok_or_else(|| {
        NaviError::Internal("edit end is not a character boundary in the original file".to_owned())
    })?;

    let ending = line_ending_at(content, original_start);
    let inserted = if ending == "\n" {
        new_text.clone()
    } else {
        new_text.replace('\n', ending)
    };

    let mut rewritten = String::with_capacity(content.len().saturating_add(inserted.len()));
    let mut ends_with_cr = false;
    push_preserving_seam(&mut rewritten, prefix, &mut ends_with_cr);
    push_preserving_seam(&mut rewritten, &inserted, &mut ends_with_cr);
    push_preserving_seam(&mut rewritten, suffix, &mut ends_with_cr);

    let mut final_content = String::with_capacity(bom.len().saturating_add(rewritten.len()));
    final_content.push_str(bom);
    final_content.push_str(&rewritten);
    fs::write(&resolved, final_content.as_bytes()).map_err(|err| edit_error(&err, &resolved))?;

    Ok(EditToolOutput {
        path: args.path.clone(),
        replacements: 1,
        unified_diff,
    })
}

/// Builds a unified diff of `old` and `new`, labelling both sides with `path_label`.
///
/// Inputs are compared as LF-separated lines. Each hunk shows three lines of surrounding context;
/// inputs whose line counts make a full LCS table too large fall back to a single hunk around the
/// common prefix and suffix.
pub fn compute_unified_diff(old: &str, new: &str, path_label: &str) -> String {
    let old_lines = split_diff_lines(old);
    let new_lines = split_diff_lines(new);
    format_unified_diff(&diff_ops(&old_lines, &new_lines), path_label)
}

/// Splits text into lines without terminating newlines, ignoring a final trailing newline.
fn split_diff_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// A single line-level edit operation in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

/// Computes line operations, using an LCS table for small inputs and prefix/suffix otherwise.
fn diff_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffOp<'a>> {
    if old.len().saturating_mul(new.len()) <= LCS_CELL_LIMIT {
        lcs_diff(old, new)
    } else {
        prefix_suffix_diff(old, new)
    }
}

/// Computes line operations from a suffix LCS table.
fn lcs_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffOp<'a>> {
    let rows = old.len() + 1;
    let columns = new.len() + 1;
    let mut table = vec![0u32; rows * columns];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            let value = if old[i] == new[j] {
                table[(i + 1) * columns + (j + 1)] + 1
            } else {
                table[(i + 1) * columns + j].max(table[i * columns + (j + 1)])
            };
            table[i * columns + j] = value;
        }
    }

    let mut ops = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            ops.push(DiffOp::Equal(old[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * columns + j] >= table[i * columns + (j + 1)] {
            ops.push(DiffOp::Delete(old[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Insert(new[j]));
            j += 1;
        }
    }
    while i < old.len() {
        ops.push(DiffOp::Delete(old[i]));
        i += 1;
    }
    while j < new.len() {
        ops.push(DiffOp::Insert(new[j]));
        j += 1;
    }
    ops
}

/// Computes a single changed region from the common prefix and suffix of both line lists.
fn prefix_suffix_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffOp<'a>> {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    let mut ops = Vec::with_capacity(old.len().saturating_add(new.len()));
    for &line in &old[..prefix] {
        ops.push(DiffOp::Equal(line));
    }
    for &line in &old[prefix..old_end] {
        ops.push(DiffOp::Delete(line));
    }
    for &line in &new[prefix..new_end] {
        ops.push(DiffOp::Insert(line));
    }
    for &line in &old[old_end..] {
        ops.push(DiffOp::Equal(line));
    }
    ops
}

/// Renders line operations as a unified patch with context around each change group.
fn format_unified_diff(ops: &[DiffOp<'_>], path_label: &str) -> String {
    let mut out = String::new();
    out.push_str("--- ");
    out.push_str(path_label);
    out.push('\n');
    out.push_str("+++ ");
    out.push_str(path_label);
    out.push('\n');

    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(index, op)| match op {
            DiffOp::Equal(_) => None,
            DiffOp::Delete(_) | DiffOp::Insert(_) => Some(index),
        })
        .collect();
    if changes.is_empty() {
        return out;
    }

    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut first = changes[0];
    let mut last = changes[0];
    for &change in changes.iter().skip(1) {
        // Every op between two changes is equal; keep nearby changes in one hunk.
        if change - last - 1 <= DIFF_CONTEXT_LINES * 2 {
            last = change;
        } else {
            groups.push((first, last));
            first = change;
            last = change;
        }
    }
    groups.push((first, last));

    for (first_change, last_change) in groups {
        let hunk_start = first_change.saturating_sub(DIFF_CONTEXT_LINES);
        let hunk_end = (last_change + DIFF_CONTEXT_LINES + 1).min(ops.len());

        let mut old_before = 0usize;
        let mut new_before = 0usize;
        for op in &ops[..hunk_start] {
            match op {
                DiffOp::Equal(_) => {
                    old_before += 1;
                    new_before += 1;
                }
                DiffOp::Delete(_) => old_before += 1,
                DiffOp::Insert(_) => new_before += 1,
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for op in &ops[hunk_start..hunk_end] {
            match op {
                DiffOp::Equal(_) => {
                    old_count += 1;
                    new_count += 1;
                }
                DiffOp::Delete(_) => old_count += 1,
                DiffOp::Insert(_) => new_count += 1,
            }
        }

        out.push_str("@@ -");
        out.push_str(&diff_range(old_before, old_count));
        out.push_str(" +");
        out.push_str(&diff_range(new_before, new_count));
        out.push_str(" @@\n");
        for op in &ops[hunk_start..hunk_end] {
            match op {
                DiffOp::Equal(line) => {
                    out.push(' ');
                    out.push_str(line);
                    out.push('\n');
                }
                DiffOp::Delete(line) => {
                    out.push('-');
                    out.push_str(line);
                    out.push('\n');
                }
                DiffOp::Insert(line) => {
                    out.push('+');
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Formats a unified diff range: zero-length ranges use the position before the hunk.
fn diff_range(lines_before: usize, count: usize) -> String {
    match count {
        0 => format!("{lines_before},0"),
        1 => (lines_before + 1).to_string(),
        _ => format!("{},{}", lines_before + 1, count),
    }
}

/// Converts CRLF and lone CR line breaks to LF.
fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Maps each LF-normalized byte offset to the offset it occupies in the original text.
fn normalized_offset_map(content: &str) -> Vec<usize> {
    let bytes = content.as_bytes();
    let mut map = Vec::with_capacity(bytes.len() + 1);
    let mut offset = 0;
    while offset < bytes.len() {
        map.push(offset);
        if bytes[offset] == b'\r' {
            offset += 1;
            if offset < bytes.len() && bytes[offset] == b'\n' {
                offset += 1;
            }
        } else {
            offset += 1;
        }
    }
    map.push(bytes.len());
    map
}

/// Line ending used by the line that contains `offset`, falling back to the file's dominant CR.
fn line_ending_at(content: &str, offset: usize) -> &'static str {
    let bytes = content.as_bytes();
    let start = offset.min(bytes.len());
    let forward = bytes[start..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|index| start + index);
    let line_feed = forward.or_else(|| bytes[..start].iter().rposition(|&byte| byte == b'\n'));
    match line_feed {
        Some(index) if index > 0 && bytes[index - 1] == b'\r' => "\r\n",
        Some(_) => "\n",
        None if bytes.contains(&b'\r') => "\r",
        None => "\n",
    }
}

/// Appends a part, inserting an LF when a trailing lone CR would merge with a leading LF.
fn push_preserving_seam(out: &mut String, part: &str, ends_with_cr: &mut bool) {
    if part.is_empty() {
        return;
    }
    if *ends_with_cr && part.starts_with('\n') {
        out.push('\n');
    }
    out.push_str(part);
    *ends_with_cr = part.ends_with('\r');
}

/// Resolves a caller path, rejecting empty paths, NUL bytes, and escapes from the workspace.
fn resolve_workspace_path(workspace_root: &Path, user_path: &str) -> NaviResult<PathBuf> {
    if user_path.is_empty() {
        return Err(NaviError::InvalidInput(
            "edit path must not be empty".to_owned(),
        ));
    }
    if user_path.as_bytes().contains(&0) {
        return Err(NaviError::InvalidInput(format!(
            "edit path contains a NUL byte: {user_path:?}"
        )));
    }

    let root_abs = absolute_path(workspace_root)?;
    let canonical_root = fs::canonicalize(&root_abs).map_err(|err| edit_error(&err, &root_abs))?;
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
    if !candidate.starts_with(&root_abs) && !candidate.starts_with(&canonical_root) {
        return Err(path_escape_error(user_path, workspace_root));
    }

    // A path inside the workspace can still reach outside it through a symlink. A missing file
    // is left to `fs::read` so callers get a precise not-found error.
    match fs::canonicalize(&candidate) {
        Ok(resolved) => {
            if !resolved.starts_with(&canonical_root) {
                return Err(path_escape_error(user_path, workspace_root));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(edit_error(&err, &candidate)),
    }

    Ok(candidate)
}

/// Makes a path absolute without touching the filesystem beyond the current directory.
fn absolute_path(path: &Path) -> NaviResult<PathBuf> {
    if path.is_absolute() {
        normalize_absolute(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|err| NaviError::Io(format!("cannot read current directory: {err}")))?;
        normalize_absolute(&cwd.join(path))
    }
}

/// Collapses `.` and `..` in a relative path, refusing to leave the workspace.
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

/// Builds the error used for every workspace containment failure.
fn path_escape_error(user_path: &str, workspace_root: &Path) -> NaviError {
    NaviError::InvalidInput(format!(
        "path '{user_path}' escapes workspace root '{}'",
        workspace_root.display()
    ))
}

/// Maps an I/O failure to the corresponding shared error kind.
fn edit_error(err: &io::Error, path: &Path) -> NaviError {
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique workspace directory that is removed when the test ends.
    struct TempWorkspace {
        root: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("navi-tool-edit-{}-{id}", std::process::id()));
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

        fn args(&self, path: &str, old_text: &str, new_text: &str) -> EditToolArgs {
            EditToolArgs {
                path: path.to_owned(),
                old_text: old_text.to_owned(),
                new_text: new_text.to_owned(),
            }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn replaces_unique_match_and_returns_clean_diff() {
        let workspace = TempWorkspace::new();
        workspace.write("note.txt", b"alpha\nbeta\ngamma\n");

        let output = execute_edit(&workspace.root, &workspace.args("note.txt", "beta", "BETA"))
            .expect("edit unique text");

        assert_eq!(output.path, "note.txt");
        assert_eq!(output.replacements, 1);
        assert_eq!(
            fs::read_to_string(workspace.root.join("note.txt")).expect("read edited file"),
            "alpha\nBETA\ngamma\n"
        );
        assert!(
            output
                .unified_diff
                .starts_with("--- note.txt\n+++ note.txt\n")
        );
        assert!(output.unified_diff.contains("@@ "));
        assert!(output.unified_diff.contains("-beta\n"));
        assert!(output.unified_diff.contains("+BETA\n"));
        assert!(output.unified_diff.contains(" alpha\n"));
    }

    #[test]
    fn missing_text_is_rejected_without_writing() {
        let workspace = TempWorkspace::new();
        workspace.write("note.txt", b"alpha\nbeta\n");

        let error = execute_edit(
            &workspace.root,
            &workspace.args("note.txt", "missing", "replacement"),
        )
        .expect_err("missing text must be rejected");

        assert!(matches!(error, NaviError::InvalidInput(_)), "{error}");
        assert_eq!(
            fs::read_to_string(workspace.root.join("note.txt")).expect("read file"),
            "alpha\nbeta\n"
        );
    }

    #[test]
    fn ambiguous_text_is_rejected_without_writing() {
        let workspace = TempWorkspace::new();
        workspace.write("dup.txt", b"same\nother\nsame\n");

        let error = execute_edit(
            &workspace.root,
            &workspace.args("dup.txt", "same", "unique"),
        )
        .expect_err("ambiguous text must be rejected");

        assert!(matches!(error, NaviError::InvalidInput(_)), "{error}");
        assert!(error.to_string().contains("unique"));
        assert_eq!(
            fs::read_to_string(workspace.root.join("dup.txt")).expect("read file"),
            "same\nother\nsame\n"
        );
    }

    #[test]
    fn preserves_crlf_endings_outside_and_inside_the_replacement() {
        let workspace = TempWorkspace::new();
        workspace.write("crlf.txt", b"one\r\ntwo\r\nthree\r\n");

        execute_edit(
            &workspace.root,
            &workspace.args("crlf.txt", "two", "second\nline"),
        )
        .expect("edit CRLF file");

        let content = fs::read_to_string(workspace.root.join("crlf.txt")).expect("read CRLF file");
        assert_eq!(content, "one\r\nsecond\r\nline\r\nthree\r\n");
        assert!(!content.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn converts_new_text_line_endings_to_the_file_style() {
        let workspace = TempWorkspace::new();
        workspace.write("lf.txt", b"before\nafter\n");

        execute_edit(
            &workspace.root,
            &workspace.args("lf.txt", "before", "first\r\nsecond"),
        )
        .expect("edit LF file with CRLF new text");

        let content = fs::read_to_string(workspace.root.join("lf.txt")).expect("read LF file");
        assert_eq!(content, "first\nsecond\nafter\n");
        assert!(!content.contains('\r'));
    }

    #[test]
    fn preserves_lone_cr_line_endings() {
        let workspace = TempWorkspace::new();
        workspace.write("mac.txt", b"a\rb\rc");

        execute_edit(&workspace.root, &workspace.args("mac.txt", "b", "B\nc"))
            .expect("edit classic-Mac file");

        let content = fs::read_to_string(workspace.root.join("mac.txt")).expect("read edited file");
        assert_eq!(content, "a\rB\rc\rc");
        assert!(!content.contains('\n'));
    }

    #[test]
    fn keeps_a_lone_cr_from_merging_with_an_inserted_lf() {
        let workspace = TempWorkspace::new();
        workspace.write("seam.txt", b"a\rb\nc");

        execute_edit(&workspace.root, &workspace.args("seam.txt", "b", "\nX"))
            .expect("edit across a CR seam");

        let content =
            fs::read_to_string(workspace.root.join("seam.txt")).expect("read edited file");
        assert_eq!(content, "a\r\n\nX\nc");
    }

    #[test]
    fn preserves_bom_and_excludes_it_from_matching() {
        let workspace = TempWorkspace::new();
        workspace.write("bom.txt", b"\xef\xbb\xbfhello world\n");

        execute_edit(&workspace.root, &workspace.args("bom.txt", "world", "Rust"))
            .expect("edit file with BOM");

        let content = fs::read_to_string(workspace.root.join("bom.txt")).expect("read edited file");
        assert_eq!(content, "\u{feff}hello Rust\n");
    }

    #[test]
    fn rejects_paths_outside_the_workspace() {
        let workspace = TempWorkspace::new();

        for path in ["../escape.txt", "nested/../../escape.txt", "/etc/passwd"] {
            let error = execute_edit(&workspace.root, &workspace.args(path, "a", "b"))
                .expect_err("paths outside the workspace must be rejected");
            assert!(
                matches!(error, NaviError::InvalidInput(_)),
                "{path}: {error}"
            );
        }
        let escape = workspace
            .root
            .parent()
            .expect("workspace parent")
            .join("escape.txt");
        assert!(!escape.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_pointing_outside_the_workspace() {
        let workspace = TempWorkspace::new();
        let outside = workspace.root.with_extension("outside");
        fs::write(&outside, b"secret").expect("write outside file");
        std::os::unix::fs::symlink(&outside, workspace.root.join("link.txt"))
            .expect("create symlink");

        let error = execute_edit(
            &workspace.root,
            &workspace.args("link.txt", "secret", "changed"),
        )
        .expect_err("symlink escape must be rejected");

        assert!(matches!(error, NaviError::InvalidInput(_)), "{error}");
        assert_eq!(
            fs::read_to_string(&outside).expect("read outside file"),
            "secret"
        );
        fs::remove_file(&outside).expect("remove outside file");
    }

    #[test]
    fn rejects_empty_and_identical_replacements() {
        let workspace = TempWorkspace::new();
        workspace.write("note.txt", b"content\n");

        for (old_text, new_text) in [("", "x"), ("content", "content")] {
            let error = execute_edit(
                &workspace.root,
                &workspace.args("note.txt", old_text, new_text),
            )
            .expect_err("empty or no-op edits must be rejected");
            assert!(matches!(error, NaviError::InvalidInput(_)), "{error}");
        }
        assert_eq!(
            fs::read_to_string(workspace.root.join("note.txt")).expect("read file"),
            "content\n"
        );
    }

    #[test]
    fn missing_file_maps_to_not_found() {
        let workspace = TempWorkspace::new();

        let error = execute_edit(&workspace.root, &workspace.args("missing.txt", "a", "b"))
            .expect_err("missing file must be reported");

        assert!(matches!(error, NaviError::NotFound(_)), "{error}");
    }

    #[test]
    fn unified_diff_groups_separate_changes_into_separate_hunks() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let new = "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n";

        let diff = compute_unified_diff(old, new, "file.txt");

        assert_eq!(diff.matches("@@ ").count(), 2);
        assert!(diff.contains("-one\n"));
        assert!(diff.contains("+ONE\n"));
        assert!(diff.contains("-ten\n"));
        assert!(diff.contains("+TEN\n"));
        assert!(diff.contains(" two\n"));
    }

    #[test]
    fn args_and_output_roundtrip_through_json() {
        let args = EditToolArgs {
            path: "file.txt".to_owned(),
            old_text: "old".to_owned(),
            new_text: "new".to_owned(),
        };
        let json = serde_json::to_string(&args).expect("serialize args");
        let decoded: EditToolArgs = serde_json::from_str(&json).expect("deserialize args");
        assert_eq!(decoded, args);

        let camel: EditToolArgs =
            serde_json::from_str(r#"{"path":"file.txt","oldText":"a","newText":"b"}"#)
                .expect("deserialize camel-case aliases");
        assert_eq!(camel.old_text, "a");
        assert_eq!(camel.new_text, "b");

        let output = EditToolOutput {
            path: "file.txt".to_owned(),
            replacements: 1,
            unified_diff: "--- file.txt\n+++ file.txt\n@@ -1 +1 @@\n-old\n+new\n".to_owned(),
        };
        let json = serde_json::to_string(&output).expect("serialize output");
        let decoded: EditToolOutput = serde_json::from_str(&json).expect("deserialize output");
        assert_eq!(decoded, output);
    }
}
