//! Tamper-evident, append-only audit log stored as newline-delimited JSON.
//!
//! Every record carries the SHA-256 digest of its predecessor. The digest
//! covers all record fields in declaration order, so editing, removing, or
//! reordering any line of the backing file breaks the chain and is reported
//! by [`verify_chain`].
//!
//! The SHA-256 implementation below is self-contained (FIPS 180-4, no
//! dependencies) so that the audit trail does not inherit the trust boundary
//! of a third-party crypto crate.

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Predecessor digest used by the first record of a chain (64 zero hex digits).
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A single entry in the audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Zero-based position of the record in the chain.
    pub seq: u64,
    /// Wall-clock time of the event, in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Principal that caused the event.
    pub actor: String,
    /// Operation that was attempted.
    pub action: String,
    /// Resource the operation targeted.
    pub target: String,
    /// Result of the operation.
    pub outcome: String,
    /// Hex digest of the previous record, or [`GENESIS_HASH`] for the first one.
    pub prev_hash: String,
    /// Hex digest commits to every other field plus `prev_hash`.
    pub hash: String,
}

/// Append-only handle to a JSONL audit log.
///
/// Opening an existing file validates the whole chain before any new record
/// is appended, so a handle never extends a corrupted trail.
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    last_hash: String,
    next_seq: u64,
}

impl AuditLog {
    /// Opens an existing log after verifying it, or creates an empty log.
    ///
    /// Missing parent directories are created. A corrupt file is rejected
    /// rather than silently extended.
    pub fn open(path: impl AsRef<Path>) -> NaviResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            return Ok(Self {
                path,
                last_hash: GENESIS_HASH.to_owned(),
                next_seq: 0,
            });
        }
        let mut last: Option<AuditRecord> = None;
        scan_chain(&path, |record| last = Some(record.clone()))?;
        let (last_hash, next_seq) = match last {
            Some(record) => (record.hash, next_sequence(record.seq)?),
            None => (GENESIS_HASH.to_owned(), 0),
        };
        Ok(Self {
            path,
            last_hash,
            next_seq,
        })
    }

    /// Alias for [`AuditLog::open`].
    pub fn new(path: impl AsRef<Path>) -> NaviResult<Self> {
        Self::open(path)
    }

    /// Appends a record with an explicit timestamp and returns it.
    pub fn append(
        &mut self,
        timestamp_ms: u64,
        actor: impl AsRef<str>,
        action: impl AsRef<str>,
        target: impl AsRef<str>,
        outcome: impl AsRef<str>,
    ) -> NaviResult<AuditRecord> {
        let actor = actor.as_ref();
        let action = action.as_ref();
        let target = target.as_ref();
        let outcome = outcome.as_ref();
        let seq = self.next_seq;
        let hash = compute_record_hash(
            &self.last_hash,
            seq,
            timestamp_ms,
            actor,
            action,
            target,
            outcome,
        );
        let record = AuditRecord {
            seq,
            timestamp_ms,
            actor: actor.to_owned(),
            action: action.to_owned(),
            target: target.to_owned(),
            outcome: outcome.to_owned(),
            prev_hash: self.last_hash.clone(),
            hash,
        };
        let line = serde_json::to_string(&record)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        file.flush()?;
        self.next_seq = next_sequence(seq)?;
        self.last_hash = record.hash.clone();
        Ok(record)
    }

    /// Appends a record using the current wall-clock time in milliseconds.
    pub fn append_event(
        &mut self,
        actor: impl AsRef<str>,
        action: impl AsRef<str>,
        target: impl AsRef<str>,
        outcome: impl AsRef<str>,
    ) -> NaviResult<AuditRecord> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| NaviError::Internal(format!("system clock error: {err}")))?;
        let timestamp_ms = u64::try_from(elapsed.as_millis())
            .map_err(|_| NaviError::Internal("audit timestamp out of range".to_owned()))?;
        self.append(timestamp_ms, actor, action, target, outcome)
    }

    /// Reads and validates every record currently in the log.
    pub fn records(&self) -> NaviResult<Vec<AuditRecord>> {
        let mut records = Vec::new();
        scan_chain(&self.path, |record| records.push(record.clone()))?;
        Ok(records)
    }

    /// Verifies the backing file and returns the number of records in it.
    pub fn verify(&self) -> NaviResult<usize> {
        verify_chain(&self.path)
    }

    /// Path of the backing JSONL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Digest that the next appended record will reference.
    pub fn last_hash(&self) -> &str {
        &self.last_hash
    }

    /// Sequence number that the next appended record will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Number of records appended through this handle.
    pub fn len(&self) -> usize {
        usize::try_from(self.next_seq).unwrap_or(usize::MAX)
    }

    /// Whether this handle has appended no records.
    pub fn is_empty(&self) -> bool {
        self.next_seq == 0
    }
}

/// Verifies a JSONL audit log and returns its record count.
///
/// Each line must parse as an [`AuditRecord`], continue the sequence from
/// zero, reference the previous line's digest, and carry a digest that
/// matches its own fields. Any tampered, deleted, duplicated, or reordered
/// line therefore produces an error. Blank lines are ignored.
pub fn verify_chain(path: impl AsRef<Path>) -> NaviResult<usize> {
    scan_chain(path.as_ref(), |_| {})
}

/// Computes the hex digest committing to one record's fields.
///
/// The preimage is the concatenation, in order, of `prev_hash` (64 hex
/// digits), `seq` and `timestamp_ms` (decimal), then `actor`, `action`,
/// `target`, and `outcome` (raw UTF-8 bytes), matching the chaining rule
/// `sha256(prev_hash || seq || timestamp_ms || actor || action || target ||
/// outcome)`.
pub fn compute_record_hash(
    prev_hash: &str,
    seq: u64,
    timestamp_ms: u64,
    actor: &str,
    action: &str,
    target: &str,
    outcome: &str,
) -> String {
    let material = format!("{prev_hash}{seq}{timestamp_ms}{actor}{action}{target}{outcome}");
    sha256_hex(material.as_bytes())
}

/// Computes the FIPS 180-4 SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = INITIAL_STATE;
    // Every input needs at least one length byte and eight length bytes; the
    // mask rounds the capacity up to a whole 64-byte block.
    let mut padded = Vec::with_capacity(data.len().saturating_add(72) & !63);
    padded.extend_from_slice(data);
    padded.push(0x80);
    let remainder = padded.len() & 63;
    let zero_count = if remainder <= 56 {
        56 - remainder
    } else {
        120 - remainder
    };
    padded.resize(padded.len() + zero_count, 0);
    let bit_length = u64::try_from(data.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    padded.extend_from_slice(&bit_length.to_be_bytes());
    debug_assert_eq!(padded.len() & 63, 0);

    for block in padded.as_chunks::<64>().0 {
        compress(&mut state, block);
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        let start = index * 4;
        digest[start..start + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Computes the lowercase hex representation of a SHA-256 digest.
pub fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&sha256(data))
}

/// SHA-256 round constants (first 32 bits of the fractional parts of cube
/// roots of the first 64 primes).
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Initial working variables (fractional parts of square roots of the first
/// eight primes).
const INITIAL_STATE: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Verifies the chain line by line and forwards each validated record.
fn scan_chain(path: &Path, mut on_record: impl FnMut(&AuditRecord)) -> NaviResult<usize> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(NaviError::NotFound(format!(
                "audit log not found: {}",
                path.display()
            )));
        }
        Err(err) => return Err(err.into()),
    };
    let reader = BufReader::new(file);
    let mut expected_seq = 0u64;
    let mut expected_prev = GENESIS_HASH.to_owned();
    let mut count = 0usize;

    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let line_number = index + 1;
        let record: AuditRecord = serde_json::from_str(line).map_err(|err| {
            NaviError::Protocol(format!("invalid audit record at line {line_number}: {err}"))
        })?;
        if record.seq != expected_seq {
            return Err(NaviError::InvalidInput(format!(
                "audit sequence mismatch at line {line_number}: expected {expected_seq}, got {}",
                record.seq
            )));
        }
        if !hashes_equal(&record.prev_hash, &expected_prev) {
            return Err(NaviError::InvalidInput(format!(
                "audit predecessor mismatch at line {line_number}"
            )));
        }
        let expected_hash = compute_record_hash(
            &record.prev_hash,
            record.seq,
            record.timestamp_ms,
            &record.actor,
            &record.action,
            &record.target,
            &record.outcome,
        );
        if !hashes_equal(&record.hash, &expected_hash) {
            return Err(NaviError::InvalidInput(format!(
                "audit digest mismatch at line {line_number}"
            )));
        }
        expected_prev = record.hash.clone();
        expected_seq = next_sequence(expected_seq)?;
        count += 1;
        on_record(&record);
    }
    Ok(count)
}

/// Returns `seq + 1` or an error instead of overflowing.
fn next_sequence(seq: u64) -> NaviResult<u64> {
    seq.checked_add(1)
        .ok_or_else(|| NaviError::Internal("audit log sequence overflow".to_owned()))
}

/// Compares two hex digests without short-circuiting on a differing byte.
fn hashes_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (&a, &b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Converts bytes to lowercase hexadecimal.
fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// Applies one SHA-256 compression function to a 64-byte block.
fn compress(state: &mut [u32; 8], block: &[u8]) {
    let mut schedule = [0u32; 64];
    for (slot, bytes) in schedule[..16].iter_mut().zip(block.as_chunks::<4>().0) {
        *slot = u32::from_be_bytes(*bytes);
    }
    for index in 16..64 {
        let x = schedule[index - 15];
        let y = schedule[index - 2];
        let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for (index, &word) in schedule.iter().enumerate() {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ ((!e) & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(choose)
            .wrapping_add(K[index])
            .wrapping_add(word);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    let updated = [a, b, c, d, e, f, g, h];
    for (slot, addend) in state.iter_mut().zip(updated) {
        *slot = (*slot).wrapping_add(addend);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempLog {
        path: PathBuf,
    }

    impl TempLog {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "navi-audit-{label}-{}-{id}.jsonl",
                std::process::id()
            ));
            Self { path }
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256(b""),
            [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55
            ]
        );
    }

    #[test]
    fn sha256_handles_padding_boundaries_and_binary_input() {
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 100]),
            "2816597888e4a0d3a36b82b83316ab32680eb8f00f8cd3b904d681246d285a0e"
        );
        let bytes: Vec<u8> = (0u8..32).collect();
        assert_eq!(
            sha256_hex(&bytes),
            "630dcd2966c4336691125448bbb25b4ff412a49c732db2c8abc1b8581bd710dd"
        );
    }

    #[test]
    fn append_builds_a_continuous_hash_chain() {
        let log_file = TempLog::new("append");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        let first = log
            .append(1_000, "alice", "read", "doc.txt", "ok")
            .expect("first append");
        assert_eq!(first.seq, 0);
        assert_eq!(first.prev_hash, GENESIS_HASH);
        let second = log
            .append(2_000, "bob", "write", "doc.txt", "ok")
            .expect("second append");
        assert_eq!(second.seq, 1);
        assert_eq!(second.prev_hash, first.hash);
        assert_eq!(
            second.hash,
            compute_record_hash(&first.hash, 1, 2_000, "bob", "write", "doc.txt", "ok")
        );
        assert_ne!(first.hash, second.hash);
        assert_eq!(log.records().expect("records"), vec![first, second]);
    }

    #[test]
    fn verify_chain_accepts_a_valid_log() {
        let log_file = TempLog::new("valid");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        for seq in 0..5u64 {
            log.append(seq * 10, "actor", "action", "target", "ok")
                .expect("append");
        }
        assert_eq!(verify_chain(&log_file.path).expect("verify"), 5);
        assert_eq!(log.verify().expect("verify"), 5);
        assert_eq!(log.len(), 5);
        assert!(!log.is_empty());
    }

    #[test]
    fn verify_chain_detects_a_tampered_field() {
        let log_file = TempLog::new("tampered");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        log.append(1, "alice", "read", "doc.txt", "ok")
            .expect("append");
        log.append(2, "bob", "write", "doc.txt", "ok")
            .expect("append");
        let contents = fs::read_to_string(&log_file.path).expect("read");
        let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
        let mut record: AuditRecord = serde_json::from_str(&lines[1]).expect("parse");
        record.outcome = "denied".to_owned();
        lines[1] = serde_json::to_string(&record).expect("serialize");
        fs::write(&log_file.path, format!("{}\n", lines.join("\n"))).expect("write");
        assert!(verify_chain(&log_file.path).is_err());
    }

    #[test]
    fn verify_chain_detects_a_deleted_line() {
        let log_file = TempLog::new("deleted");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        for seq in 0..3u64 {
            log.append(seq, "actor", "action", "target", "ok")
                .expect("append");
        }
        let contents = fs::read_to_string(&log_file.path).expect("read");
        let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
        lines.remove(1);
        fs::write(&log_file.path, format!("{}\n", lines.join("\n"))).expect("write");
        assert!(verify_chain(&log_file.path).is_err());
    }

    #[test]
    fn verify_chain_detects_reordered_lines() {
        let log_file = TempLog::new("reordered");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        for seq in 0..3u64 {
            log.append(seq, "actor", "action", "target", "ok")
                .expect("append");
        }
        let contents = fs::read_to_string(&log_file.path).expect("read");
        let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
        lines.reverse();
        fs::write(&log_file.path, format!("{}\n", lines.join("\n"))).expect("write");
        assert!(verify_chain(&log_file.path).is_err());
    }

    #[test]
    fn verify_chain_detects_a_duplicated_line() {
        let log_file = TempLog::new("duplicate");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        log.append(1, "alice", "read", "doc.txt", "ok")
            .expect("first append");
        log.append(2, "bob", "write", "doc.txt", "ok")
            .expect("second append");
        let contents = fs::read_to_string(&log_file.path).expect("read");
        let mut lines: Vec<String> = contents.lines().map(str::to_owned).collect();
        // Duplicating the first line breaks both the sequence and the hash
        // linkage, so verification must reject the file.
        let duplicate = lines[0].clone();
        lines.insert(1, duplicate);
        fs::write(&log_file.path, format!("{}\n", lines.join("\n"))).expect("write");
        assert!(verify_chain(&log_file.path).is_err());
    }

    #[test]
    fn verify_chain_detects_a_forged_digest() {
        let log_file = TempLog::new("forged");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        log.append(1, "actor", "action", "target", "ok")
            .expect("append");
        let contents = fs::read_to_string(&log_file.path).expect("read");
        let mut record: AuditRecord = serde_json::from_str(contents.trim()).expect("parse");
        record.hash = "f".repeat(64);
        fs::write(
            &log_file.path,
            format!("{}\n", serde_json::to_string(&record).expect("serialize")),
        )
        .expect("write");
        assert!(verify_chain(&log_file.path).is_err());
    }

    #[test]
    fn reopening_continues_the_existing_chain() {
        let log_file = TempLog::new("reopen");
        {
            let mut log = AuditLog::new(&log_file.path).expect("create");
            log.append(10, "a", "start", "svc", "ok").expect("append");
        }
        {
            let mut log = AuditLog::open(&log_file.path).expect("reopen");
            assert_eq!(log.next_seq(), 1);
            assert_eq!(log.len(), 1);
            let record = log.append(20, "b", "stop", "svc", "ok").expect("append");
            assert_eq!(record.seq, 1);
        }
        assert_eq!(verify_chain(&log_file.path).expect("verify"), 2);
    }

    #[test]
    fn verify_chain_rejects_missing_file_and_malformed_json() {
        let log_file = TempLog::new("malformed");
        assert!(matches!(
            verify_chain(&log_file.path),
            Err(NaviError::NotFound(_))
        ));
        fs::write(&log_file.path, "{not json}\n").expect("write");
        assert!(matches!(
            verify_chain(&log_file.path),
            Err(NaviError::Protocol(_))
        ));
        fs::write(&log_file.path, "").expect("write");
        assert_eq!(verify_chain(&log_file.path).expect("empty log"), 0);
    }

    #[test]
    fn record_hash_binds_every_field() {
        let base = compute_record_hash(GENESIS_HASH, 0, 1, "a", "b", "c", "d");
        assert_eq!(
            base,
            compute_record_hash(GENESIS_HASH, 0, 1, "a", "b", "c", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(&"1".repeat(64), 0, 1, "a", "b", "c", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(GENESIS_HASH, 1, 1, "a", "b", "c", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(GENESIS_HASH, 0, 2, "a", "b", "c", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(GENESIS_HASH, 0, 1, "aa", "b", "c", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(GENESIS_HASH, 0, 1, "a", "aa", "c", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(GENESIS_HASH, 0, 1, "a", "b", "aa", "d")
        );
        assert_ne!(
            base,
            compute_record_hash(GENESIS_HASH, 0, 1, "a", "b", "c", "aa")
        );
        assert_eq!(base.len(), 64);
    }

    #[test]
    fn verify_chain_tolerates_crlf_and_blank_lines() {
        let log_file = TempLog::new("crlf");
        let mut log = AuditLog::open(&log_file.path).expect("open");
        log.append(1, "actor", "action", "target", "ok")
            .expect("append");
        log.append(2, "actor", "action", "target", "ok")
            .expect("append");
        let contents = fs::read_to_string(&log_file.path).expect("read");
        let converted = format!("\r\n{}\r\n", contents.replace('\n', "\r\n").trim_end());
        fs::write(&log_file.path, converted).expect("write");
        assert_eq!(verify_chain(&log_file.path).expect("verify"), 2);
    }
}
