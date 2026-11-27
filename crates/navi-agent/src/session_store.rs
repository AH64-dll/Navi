//! Append-only JSONL session transcript store with tree branching.
//!
//! Ported from `packages/coding-agent/src/core/session-manager.ts`. Entries are
//! never rewritten: each entry carries an id and a parent id, appending adds a
//! child of the current leaf by default, and branching only moves the leaf
//! pointer. Lines that cannot be parsed (including an unterminated trailing
//! fragment written by a process that died mid-write) are skipped on load.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use navi_types::{ContentBlock, Message, NaviError, NaviResult, Role};
use serde::{Deserialize, Serialize};

/// One immutable node in a session's entry tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Unique id within the session.
    pub id: String,
    /// Parent entry id; `None` marks a root entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Creation time in milliseconds since the Unix epoch.
    #[serde(default)]
    pub timestamp_ms: u64,
    /// Payload of this entry, tagged by `type` in the JSONL wire format.
    #[serde(flatten)]
    pub kind: SessionEntryKind,
}

impl SessionEntry {
    /// Creates an entry with the supplied identity and payload.
    pub fn new(
        id: impl Into<String>,
        parent_id: Option<String>,
        timestamp_ms: u64,
        kind: SessionEntryKind,
    ) -> Self {
        Self {
            id: id.into(),
            parent_id,
            timestamp_ms,
            kind,
        }
    }
}

/// Payload of a [`SessionEntry`].
///
/// The wire tag is `type` to stay compatible with Pi session files. State-only
/// kinds (`ModelChange`, `Label`) are persisted but do not contribute messages
/// to a linearized transcript; `Compaction` contributes its summary as a system
/// message and drops the messages it summarizes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntryKind {
    /// A regular conversation message.
    Message {
        /// The message to include in the transcript.
        message: Message,
    },
    /// A checkpoint that summarizes earlier messages.
    Compaction {
        /// Summary text carried forward into the transcript.
        summary: String,
        /// Id of the first entry kept after the compaction boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        /// Tokens counted before the summary was produced.
        #[serde(default)]
        tokens_before: u64,
    },
    /// A change of the model used for subsequent turns.
    ModelChange {
        /// Provider identifier.
        provider: String,
        /// Model identifier within the provider.
        model_id: String,
    },
    /// A user-defined bookmark on another entry.
    Label {
        /// Id of the labeled entry.
        target_id: String,
        /// Label text, or `None` to clear a previous label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// An append-only session transcript backed by a JSONL file.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    entries: Vec<SessionEntry>,
    by_id: BTreeMap<String, usize>,
    leaf_id: Option<String>,
    ends_with_newline: bool,
}

impl SessionStore {
    /// Opens a store at `path`.
    ///
    /// An existing file is parsed tolerantly; a missing file yields an empty
    /// store that creates the file on the first append.
    pub fn open(path: impl AsRef<Path>) -> NaviResult<Self> {
        let mut store = Self {
            path: path.as_ref().to_path_buf(),
            entries: Vec::new(),
            by_id: BTreeMap::new(),
            leaf_id: None,
            ends_with_newline: true,
        };
        match fs::read(&store.path) {
            Ok(bytes) => store.apply_file(&bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(store)
    }

    /// Returns the path of the backing JSONL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the entries currently held in memory, in append order.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Returns the id of the current leaf, if the session has any entries.
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// Looks up an entry by id.
    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }

    /// Reparses the backing file and returns its entries without changing the
    /// in-memory store. Blank, malformed, and duplicate-id lines are skipped.
    pub fn load_entries(&self) -> NaviResult<Vec<SessionEntry>> {
        load_jsonl(&self.path)
    }

    /// Reparses the backing file into the in-memory store and resets the leaf
    /// to the last valid entry.
    pub fn reload(&mut self) -> NaviResult<()> {
        let bytes = fs::read(&self.path)?;
        self.apply_file(&bytes);
        Ok(())
    }

    /// Appends an entry and advances the leaf to it, returning its id.
    ///
    /// A missing `parent_id` is filled with the current leaf, so branching
    /// followed by an append creates a child of the selected entry. An explicit
    /// `parent_id` allows appending to any entry already on disk. Nothing is
    /// changed in memory when the write fails.
    pub fn append(&mut self, mut entry: SessionEntry) -> NaviResult<String> {
        if entry.id.trim().is_empty() {
            return Err(NaviError::InvalidInput(
                "session entry id must not be empty".to_owned(),
            ));
        }
        if self.by_id.contains_key(&entry.id) {
            return Err(NaviError::InvalidInput(format!(
                "duplicate session entry id '{}'",
                entry.id
            )));
        }
        if entry.parent_id.is_none() {
            entry.parent_id = self.leaf_id.clone();
        }
        match entry.parent_id.as_deref() {
            Some(parent) if parent == entry.id => {
                return Err(NaviError::InvalidInput(format!(
                    "entry '{}' cannot be its own parent",
                    entry.id
                )));
            }
            Some(parent) if !self.by_id.contains_key(parent) => {
                return Err(NaviError::NotFound(format!(
                    "parent entry '{parent}' not found"
                )));
            }
            _ => {}
        }

        let line = serde_json::to_string(&entry)?;
        self.write_line(&line)?;

        let id = entry.id.clone();
        self.by_id.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        Ok(id)
    }

    /// Moves the leaf pointer to an existing entry so the next append starts a
    /// new branch there. History is never modified.
    pub fn branch_from(&mut self, entry_id: &str) -> NaviResult<()> {
        if !self.by_id.contains_key(entry_id) {
            return Err(NaviError::NotFound(format!("entry '{entry_id}' not found")));
        }
        self.leaf_id = Some(entry_id.to_owned());
        Ok(())
    }

    /// Moves the leaf before the first entry, so the next append creates a new
    /// root.
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// Returns the path from root to `leaf_id`, in order, including every entry
    /// kind. Unknown ids and cycles in the parent chain are errors.
    pub fn branch_path(&self, leaf_id: &str) -> NaviResult<Vec<&SessionEntry>> {
        let mut current = self
            .by_id
            .get(leaf_id)
            .copied()
            .ok_or_else(|| NaviError::NotFound(format!("entry '{leaf_id}' not found")))?;
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        loop {
            let entry = &self.entries[current];
            if !visited.insert(entry.id.as_str()) {
                return Err(NaviError::Protocol(
                    "cycle in session entry parent chain".to_owned(),
                ));
            }
            path.push(entry);
            match entry.parent_id.as_deref().and_then(|id| self.by_id.get(id)) {
                Some(parent) => current = *parent,
                None => break,
            }
        }
        path.reverse();
        Ok(path)
    }

    /// Returns the model-visible messages on the path from root to `leaf_id`.
    pub fn linearize_branch(&self, leaf_id: &str) -> NaviResult<Vec<Message>> {
        let path = self.branch_path(leaf_id)?;
        Ok(messages_from_path(&path))
    }

    /// Returns the model-visible messages on the current leaf's path.
    pub fn linearize_current(&self) -> NaviResult<Vec<Message>> {
        let leaf = self
            .leaf_id
            .as_deref()
            .ok_or_else(|| NaviError::NotFound("session has no leaf entry".to_owned()))?;
        self.linearize_branch(leaf)
    }

    fn apply_file(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        self.entries = parse_entries(&text);
        self.ends_with_newline = text.is_empty() || text.ends_with('\n');
        self.by_id.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            self.by_id.entry(entry.id.clone()).or_insert(index);
        }
        self.leaf_id = self.entries.last().map(|entry| entry.id.clone());
    }

    fn write_line(&mut self, line: &str) -> NaviResult<()> {
        if let Some(parent) = self.path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        if !self.ends_with_newline {
            // Separate a valid new record from a fragment left by an interrupted write.
            file.write_all(b"\n")?;
        }
        writeln!(file, "{line}")?;
        self.ends_with_newline = true;
        Ok(())
    }
}

/// Parses a JSONL session file, skipping blank and malformed lines.
pub fn load_jsonl(path: impl AsRef<Path>) -> NaviResult<Vec<SessionEntry>> {
    let bytes = fs::read(path.as_ref())?;
    Ok(parse_entries(&String::from_utf8_lossy(&bytes)))
}

fn parse_entries(text: &str) -> Vec<SessionEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionEntry>(line) else {
            continue;
        };
        if entry.id.trim().is_empty() || !seen.insert(entry.id.clone()) {
            continue;
        }
        entries.push(entry);
    }
    entries
}

fn message_of(entry: &SessionEntry) -> Option<&Message> {
    match &entry.kind {
        SessionEntryKind::Message { message } => Some(message),
        _ => None,
    }
}

fn messages_from_path(path: &[&SessionEntry]) -> Vec<Message> {
    let compaction = path
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| match &entry.kind {
            SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => Some((index, summary.as_str(), first_kept_entry_id.as_deref())),
            _ => None,
        });
    let Some((index, summary, first_kept)) = compaction else {
        return path
            .iter()
            .filter_map(|entry| message_of(entry).cloned())
            .collect();
    };

    let mut messages = vec![Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: summary.to_owned(),
        }],
        timestamp_ms: path[index].timestamp_ms,
        stop_reason: None,
        usage: None,
        metadata: BTreeMap::new(),
    }];
    let start = first_kept
        .and_then(|id| path[..index].iter().position(|entry| entry.id == id))
        .unwrap_or(index);
    for entry in &path[start..index] {
        if let Some(message) = message_of(entry)
            && message.role != Role::System
        {
            messages.push(message.clone());
        }
    }
    for entry in &path[index + 1..] {
        if let Some(message) = message_of(entry) {
            messages.push(message.clone());
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempSession {
        root: PathBuf,
        file: PathBuf,
    }

    impl TempSession {
        fn create() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "navi-session-store-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp dir");
            Self {
                file: root.join("nested").join("session.jsonl"),
                root,
            }
        }
    }

    impl Drop for TempSession {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn entry(id: &str, parent: Option<&str>, kind: SessionEntryKind) -> SessionEntry {
        SessionEntry::new(id, parent.map(str::to_owned), 1_000, kind)
    }

    fn text_message(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        entry(
            id,
            parent,
            SessionEntryKind::Message {
                message: Message::user_text(text, 1_000),
            },
        )
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(Message::text_content).collect()
    }

    #[test]
    fn open_missing_file_appends_creates_file_and_reloads() {
        let temp = TempSession::create();
        let mut store = SessionStore::open(&temp.file).expect("open missing file");
        assert!(store.entries().is_empty());
        assert_eq!(store.leaf_id(), None);

        let first = store
            .append(text_message("e1", None, "hello"))
            .expect("append first");
        let second = store
            .append(entry(
                "e2",
                None,
                SessionEntryKind::Message {
                    message: Message::assistant_text("hi", 1_001),
                },
            ))
            .expect("append second");
        assert_eq!(first, "e1");
        assert_eq!(second, "e2");
        assert_eq!(store.leaf_id(), Some("e2"));
        assert_eq!(store.get("e1").and_then(|e| e.parent_id.as_deref()), None);
        assert_eq!(
            store.get("e2").and_then(|e| e.parent_id.as_deref()),
            Some("e1")
        );

        let raw = fs::read_to_string(&temp.file).expect("read jsonl");
        assert_eq!(raw.lines().count(), 2);
        assert!(raw.ends_with('\n'));
        assert_eq!(store.load_entries().expect("load entries"), store.entries());

        let reopened = SessionStore::open(&temp.file).expect("reopen");
        assert_eq!(reopened.entries(), store.entries());
        assert_eq!(reopened.leaf_id(), Some("e2"));
    }

    #[test]
    fn append_rejects_invalid_entries_without_mutating_state() {
        let temp = TempSession::create();
        let mut store = SessionStore::open(&temp.file).expect("open");
        store
            .append(text_message("e1", None, "root"))
            .expect("append root");

        let duplicate = store
            .append(text_message("e1", None, "duplicate"))
            .expect_err("duplicate id rejected");
        assert!(matches!(duplicate, NaviError::InvalidInput(_)));
        let blank = store
            .append(text_message("  ", None, "blank"))
            .expect_err("blank id rejected");
        assert!(matches!(blank, NaviError::InvalidInput(_)));
        let missing_parent = store
            .append(text_message("e2", Some("nope"), "orphan"))
            .expect_err("unknown parent rejected");
        assert!(matches!(missing_parent, NaviError::NotFound(_)));
        let self_parent = store
            .append(entry(
                "self",
                Some("self"),
                SessionEntryKind::ModelChange {
                    provider: "p".to_owned(),
                    model_id: "m".to_owned(),
                },
            ))
            .expect_err("self parent rejected");
        assert!(matches!(self_parent, NaviError::InvalidInput(_)));

        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.leaf_id(), Some("e1"));
        assert_eq!(
            fs::read_to_string(&temp.file)
                .expect("read jsonl")
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn tolerant_parse_skips_malformed_lines_and_repairs_partial_tail() {
        let temp = TempSession::create();
        fs::create_dir_all(temp.file.parent().expect("parent dir")).expect("create dir");
        let good = serde_json::to_string(&text_message("e1", None, "first")).expect("serialize");
        let also_good =
            serde_json::to_string(&text_message("e2", Some("e1"), "second")).expect("serialize");
        fs::write(
            &temp.file,
            format!("{good}\n\n{{not json\n{also_good}\n{{\"id\":\"partial\",\"parent_id\":\"e2\""),
        )
        .expect("write raw jsonl");

        let mut store = SessionStore::open(&temp.file).expect("open tolerant");
        assert_eq!(store.entries().len(), 2);
        assert_eq!(store.leaf_id(), Some("e2"));

        store
            .append(text_message("e3", None, "third"))
            .expect("append after fragment");
        let reloaded = store.load_entries().expect("reload from disk");
        assert_eq!(reloaded.len(), 3);
        assert_eq!(reloaded[2].id, "e3");
        assert_eq!(reloaded[2].parent_id.as_deref(), Some("e2"));
    }

    #[test]
    fn final_line_without_newline_is_loaded_and_preserved_on_append() {
        let temp = TempSession::create();
        fs::create_dir_all(temp.file.parent().expect("parent dir")).expect("create dir");
        let first = serde_json::to_string(&text_message("e1", None, "one")).expect("serialize");
        let last =
            serde_json::to_string(&text_message("e2", Some("e1"), "two")).expect("serialize");
        fs::write(&temp.file, format!("{first}\n{last}")).expect("write without final newline");

        let mut store = SessionStore::open(&temp.file).expect("open");
        assert_eq!(store.entries().len(), 2);
        store
            .append(text_message("e3", None, "three"))
            .expect("append");

        let raw = fs::read_to_string(&temp.file).expect("read jsonl");
        assert_eq!(raw.lines().count(), 3);
        let reopened = SessionStore::open(&temp.file).expect("reopen");
        assert_eq!(
            texts(&reopened.linearize_current().expect("linearize")),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn branching_moves_leaf_and_keeps_both_histories() {
        let temp = TempSession::create();
        let mut store = SessionStore::open(&temp.file).expect("open");
        store
            .append(text_message("e1", None, "root"))
            .expect("append root");
        store
            .append(text_message("e2", None, "left"))
            .expect("append left");
        store.branch_from("e1").expect("branch from root");
        store
            .append(text_message("e3", None, "right"))
            .expect("append right");

        assert_eq!(store.leaf_id(), Some("e3"));
        assert_eq!(store.branch_path("e1").expect("path to root").len(), 1);
        assert_eq!(
            texts(&store.linearize_branch("e2").expect("left branch")),
            ["root", "left"]
        );
        assert_eq!(
            texts(&store.linearize_branch("e3").expect("right branch")),
            ["root", "right"]
        );

        let reopened = SessionStore::open(&temp.file).expect("reopen");
        assert_eq!(reopened.entries().len(), 3);
        assert_eq!(
            reopened.get("e3").and_then(|e| e.parent_id.as_deref()),
            Some("e1")
        );
        store.branch_from("missing").expect_err("unknown branch id");
    }

    #[test]
    fn linearize_applies_compaction_and_skips_state_entries() {
        let temp = TempSession::create();
        let mut store = SessionStore::open(&temp.file).expect("open");
        store
            .append(text_message("e1", None, "before"))
            .expect("append e1");
        store
            .append(entry(
                "e2",
                None,
                SessionEntryKind::Message {
                    message: Message::assistant_text("kept", 1_001),
                },
            ))
            .expect("append e2");
        store
            .append(entry(
                "e3",
                None,
                SessionEntryKind::Compaction {
                    summary: "compressed".to_owned(),
                    first_kept_entry_id: Some("e2".to_owned()),
                    tokens_before: 128,
                },
            ))
            .expect("append compaction");
        store
            .append(entry(
                "e4",
                None,
                SessionEntryKind::ModelChange {
                    provider: "p".to_owned(),
                    model_id: "m".to_owned(),
                },
            ))
            .expect("append model change");
        store
            .append(entry(
                "e5",
                None,
                SessionEntryKind::Label {
                    target_id: "e1".to_owned(),
                    label: Some("bookmark".to_owned()),
                },
            ))
            .expect("append label");
        store
            .append(text_message("e6", None, "after"))
            .expect("append e6");

        let messages = store.linearize_branch("e6").expect("linearize leaf");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].text_content(), "compressed");
        assert_eq!(texts(&messages[1..]), ["kept", "after"]);
        assert_eq!(
            texts(&store.linearize_branch("e2").expect("linearize e2")),
            ["before", "kept"]
        );
        store
            .linearize_branch("absent")
            .expect_err("unknown leaf rejected");
    }

    #[test]
    fn cycle_in_loaded_parent_chain_is_reported() {
        let temp = TempSession::create();
        fs::create_dir_all(temp.file.parent().expect("parent dir")).expect("create dir");
        let a = serde_json::to_string(&SessionEntry::new(
            "a",
            Some("b".to_owned()),
            1,
            SessionEntryKind::Message {
                message: Message::user_text("a", 1),
            },
        ))
        .expect("serialize a");
        let b = serde_json::to_string(&SessionEntry::new(
            "b",
            Some("a".to_owned()),
            2,
            SessionEntryKind::Message {
                message: Message::user_text("b", 2),
            },
        ))
        .expect("serialize b");
        fs::write(&temp.file, format!("{a}\n{b}\n")).expect("write cycle");

        let store = SessionStore::open(&temp.file).expect("open cycle");
        assert!(matches!(
            store.linearize_branch("a"),
            Err(NaviError::Protocol(_))
        ));
    }

    #[test]
    fn wire_format_tags_kinds_and_roundtrips() {
        let kinds = [
            SessionEntryKind::Message {
                message: Message::user_text("hello", 7),
            },
            SessionEntryKind::Compaction {
                summary: "summary".to_owned(),
                first_kept_entry_id: None,
                tokens_before: 10,
            },
            SessionEntryKind::ModelChange {
                provider: "p".to_owned(),
                model_id: "m".to_owned(),
            },
            SessionEntryKind::Label {
                target_id: "e1".to_owned(),
                label: None,
            },
        ];
        let tags = ["message", "compaction", "model_change", "label"];
        for (kind, tag) in kinds.iter().zip(tags) {
            let record = SessionEntry::new("x", None, 3, kind.clone());
            let value = serde_json::to_value(&record).expect("serialize entry");
            assert_eq!(value["type"], tag);
            assert_eq!(value["id"], "x");
            assert_eq!(value["timestamp_ms"], 3);
            let parsed: SessionEntry = serde_json::from_value(value).expect("parse entry");
            assert_eq!(&parsed, &record);
        }
    }
}
