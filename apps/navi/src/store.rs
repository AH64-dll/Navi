//! Durable project/work graph and event history.
//!
//! A project is a row, not a settings entry. Completion is always derived from
//! the append-only `signals` ledger by `graph_score`; nothing stores a percent.

use crate::{App, Result, id, now};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL UNIQUE,
    git_root TEXT,
    git_head TEXT,
    git_dirty INTEGER NOT NULL DEFAULT 0,
    origin TEXT NOT NULL DEFAULT 'manual',
    path_state TEXT NOT NULL DEFAULT 'ok',
    archived INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS aspects (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    title TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'work',
    weight REAL NOT NULL DEFAULT 1.0,
    created INTEGER NOT NULL,
    UNIQUE(project_id, key)
);
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    aspect_id TEXT REFERENCES aspects(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'todo',
    weight REAL NOT NULL DEFAULT 1.0,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS plans (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'draft',
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS signals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id TEXT,
    aspect_id TEXT,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    value REAL,
    confidence REAL NOT NULL DEFAULT 1.0,
    evidence TEXT NOT NULL CHECK(length(trim(evidence)) > 0),
    time INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS checkpoints (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id TEXT,
    title TEXT NOT NULL,
    evidence TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY,
    epoch TEXT NOT NULL,
    project_id TEXT,
    session_id TEXT,
    time INTEGER NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    details TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    project_id TEXT,
    session_id TEXT,
    turn TEXT,
    role TEXT NOT NULL,
    parts TEXT NOT NULL,
    time INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS assistant_sessions (
    project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    model TEXT,
    agent TEXT,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS session_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session TEXT NOT NULL,
    time INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS session_tails (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    project_id TEXT,
    hash TEXT NOT NULL,
    lines INTEGER NOT NULL DEFAULT 0,
    text TEXT NOT NULL,
    time INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS window_state (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS progress_days (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    day INTEGER NOT NULL,
    percent REAL NOT NULL,
    time INTEGER NOT NULL,
    PRIMARY KEY(project_id, day)
);
"#;

pub fn migrate(db: &Connection, settings: &Value) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| error.to_string())?;
    let outcome = (|| -> Result<()> {
        db.execute_batch(SCHEMA).map_err(|error| error.to_string())?;
        for (name, declaration) in [
            ("harness", "TEXT"),
            ("native_id", "TEXT"),
            ("native_name", "TEXT"),
            ("argv", "TEXT"),
            ("env_keys", "TEXT"),
            ("launch_kind", "TEXT"),
            ("gen", "INTEGER NOT NULL DEFAULT 0"),
            ("state", "TEXT NOT NULL DEFAULT 'created'"),
            ("pid", "INTEGER"),
            ("last_active", "INTEGER"),
            ("exited", "INTEGER"),
            ("exit_status", "INTEGER"),
            ("origin", "TEXT NOT NULL DEFAULT 'navi'"),
            ("orphan", "INTEGER NOT NULL DEFAULT 0"),
            ("closed_at", "INTEGER"),
            ("replaced_by", "TEXT"),
            ("harness_version", "TEXT"),
            ("project_id", "TEXT"),
        ] {
            ensure_column(db, "sessions", name, declaration)?;
        }
        db.execute_batch(
            "CREATE INDEX IF NOT EXISTS events_project ON events(project_id, seq);
             CREATE INDEX IF NOT EXISTS events_time ON events(time);
             CREATE INDEX IF NOT EXISTS messages_project ON messages(project_id, time);
             CREATE INDEX IF NOT EXISTS signals_project ON signals(project_id, time);
             CREATE INDEX IF NOT EXISTS signals_task ON signals(task_id, time);
             CREATE INDEX IF NOT EXISTS session_events_session ON session_events(session, id);
             CREATE INDEX IF NOT EXISTS session_tails_session ON session_tails(session_id, id);",
        )
        .map_err(|error| error.to_string())?;
        seed_projects(db, settings)?;
        backfill_sessions(db)?;
        // Retention: 90 days or the newest 200k rows, whichever is smaller.
        let _ = db.execute(
            "DELETE FROM events WHERE time < ?1",
            [now().saturating_sub(90 * 86_400_000) as i64],
        );
        let _ = db.execute(
            "DELETE FROM events WHERE seq < (SELECT COALESCE(MAX(seq),0)-200000 FROM events)",
            [],
        );
        db.execute_batch("PRAGMA user_version=1")
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    match outcome {
        Ok(()) => db.execute_batch("COMMIT").map_err(|error| error.to_string()),
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn ensure_column(db: &Connection, table: &str, name: &str, declaration: &str) -> Result<()> {
    let existing = columns(db, table)?;
    if existing.iter().any(|column| column == name) {
        return Ok(());
    }
    db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {declaration}"))
        .map_err(|error| error.to_string())
}

fn columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = db
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn seed_projects(db: &Connection, settings: &Value) -> Result<()> {
    let Some(entries) = settings["projects"].as_array() else {
        return Ok(());
    };
    for entry in entries {
        let raw = entry["path"].as_str().unwrap_or_default().trim();
        if raw.is_empty() {
            continue;
        }
        let path = fs::canonicalize(raw)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        let exists: Option<String> = db
            .query_row("SELECT id FROM projects WHERE path=?1", [&path], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| error.to_string())?;
        if exists.is_some() {
            continue;
        }
        let wanted = entry["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                Path::new(&path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Project")
                    .to_string()
            });
        let name = unique_name(db, &wanted)?;
        let state = if Path::new(&path).is_dir() {
            "ok"
        } else {
            "missing"
        };
        db.execute(
            "INSERT INTO projects(id,name,path,origin,path_state,archived,created,git_dirty)
             VALUES(?1,?2,?3,'settings_migration',?4,0,?5,0)",
            params![format!("prj-{}", &id()[..8]), name, path, state, now()],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn unique_name(db: &Connection, wanted: &str) -> Result<String> {
    let mut candidate = wanted.to_string();
    let mut suffix = 2;
    loop {
        let taken: Option<i64> = db
            .query_row(
                "SELECT 1 FROM projects WHERE lower(name)=lower(?1)",
                [&candidate],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if taken.is_none() {
            return Ok(candidate);
        }
        candidate = format!("{wanted} ({suffix})");
        suffix += 1;
    }
}

fn backfill_sessions(db: &Connection) -> Result<()> {
    let mut statement = db
        .prepare("SELECT id,cwd FROM sessions WHERE project_id IS NULL OR project_id=''")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (session, cwd) in rows {
        if let Some(project) = project_id_for_path(db, &cwd) {
            db.execute(
                "UPDATE sessions SET project_id=?1 WHERE id=?2",
                params![project, session],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Resolve a cwd to the project whose path contains it; exact match first.
pub fn project_id_for_path(db: &Connection, cwd: &str) -> Option<String> {
    let canonical = fs::canonicalize(cwd)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.to_string());
    let mut statement = db
        .prepare("SELECT id,path FROM projects WHERE archived=0")
        .ok()?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .ok()?
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    if let Some((id, _)) = rows.iter().find(|(_, path)| *path == canonical) {
        return Some(id.clone());
    }
    let mut best: Option<(String, usize)> = None;
    for (id, path) in rows {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        if canonical.starts_with(&prefix)
            && best
                .as_ref()
                .is_none_or(|(_, length)| prefix.len() > *length)
        {
            best = Some((id, prefix.len()));
        }
    }
    best.map(|(id, _)| id)
}

pub fn sync_settings_projects(db: &Connection, settings: &Value) -> Result<()> {
    seed_projects(db, settings)?;
    if let Some(entries) = settings["projects"].as_array() {
        for entry in entries {
            let Some(raw) = entry["path"].as_str() else {
                continue;
            };
            let path = fs::canonicalize(raw)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| raw.to_string());
            let Some(name) = entry["name"].as_str().map(str::trim) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let current: Option<String> = db
                .query_row("SELECT name FROM projects WHERE path=?1", [&path], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(|error| error.to_string())?;
            if let Some(current) = current
                && current != name
            {
                db.execute(
                    "UPDATE projects SET name=?1 WHERE path=?2",
                    params![unique_name(db, name)?, path],
                )
                .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "item".into()
    } else {
        trimmed.chars().take(48).collect()
    }
}

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "name": row.get::<_, String>(1)?,
        "path": row.get::<_, String>(2)?,
        "git_root": row.get::<_, Option<String>>(3)?,
        "git_head": row.get::<_, Option<String>>(4)?,
        "git_dirty": row.get::<_, i64>(5)? != 0,
        "origin": row.get::<_, String>(6)?,
        "path_state": row.get::<_, String>(7)?,
        "archived": row.get::<_, i64>(8)? != 0,
        "created": row.get::<_, u64>(9)?,
    }))
}

const PROJECT_COLUMNS: &str =
    "id,name,path,git_root,git_head,git_dirty,origin,path_state,archived,created";

fn fetch_project(db: &Connection, reference: &str) -> Result<Value> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("Missing project".into());
    }
    if let Some(value) = db
        .query_row(
            &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id=?1"),
            [reference],
            project_row,
        )
        .optional()
        .map_err(|error| error.to_string())?
    {
        return Ok(value);
    }
    if let Some(value) = db
        .query_row(
            &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE lower(name)=lower(?1)"),
            [reference],
            project_row,
        )
        .optional()
        .map_err(|error| error.to_string())?
    {
        return Ok(value);
    }
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE archived=0",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if count == 1 {
        return db
            .query_row(
                &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE archived=0 LIMIT 1"),
                [],
                project_row,
            )
            .map_err(|error| error.to_string());
    }
    Err(format!("Unknown project: {reference}"))
}

#[allow(clippy::too_many_arguments)]
pub fn insert_signal(
    db: &Connection,
    project_id: &str,
    task_id: Option<&str>,
    aspect_id: Option<&str>,
    kind: &str,
    source: &str,
    value: Option<f64>,
    confidence: f64,
    evidence: &str,
) -> Result<i64> {
    if evidence.trim().is_empty() {
        return Err("Evidence is required for a signal".into());
    }
    if let Some(task) = task_id {
        let owned: Option<i64> = db
            .query_row(
                "SELECT 1 FROM tasks WHERE id=?1 AND project_id=?2",
                params![task, project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if owned.is_none() {
            return Err("Task does not belong to the project".into());
        }
    }
    db.execute(
        "INSERT INTO signals(project_id,task_id,aspect_id,kind,source,value,confidence,evidence,time)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            project_id,
            task_id,
            aspect_id,
            kind,
            source,
            value,
            confidence.clamp(0.0, 1.0),
            evidence.trim(),
            now()
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(db.last_insert_rowid())
}

struct TaskScore {
    id: String,
    title: String,
    state: String,
    weight: f64,
    score: f64,
    freshness: f64,
    verified: bool,
    covered: bool,
    unknown: bool,
    kind: String,
    source: String,
    time: u64,
}

struct SignalRow {
    kind: String,
    source: String,
    value: Option<f64>,
    confidence: f64,
    time: u64,
}

/// Evidence that does not rest on a model's opinion: the user's own verdict, a
/// test run, a commit, or a recorded checkpoint. Estimates move the number but
/// never count as verified.
fn verified_kind(kind: &str, source: &str) -> bool {
    source != "assistant"
        && matches!(
            kind,
            "verdict" | "test_pass" | "test_fail" | "commit" | "checkpoint" | "git_commit"
        )
}

/// Ten-day half-life: old evidence fades instead of expiring.
fn freshness_at(at: u64, time: u64) -> f64 {
    0.5_f64.powf(at.saturating_sub(time) as f64 / 86_400_000.0 / 10.0)
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn score_task(db: &Connection, task_id: &str, at: u64) -> Result<TaskScore> {
    let mut statement = db
        .prepare(
            "SELECT kind,source,value,confidence,time FROM signals
             WHERE task_id=?1 ORDER BY time DESC, id DESC",
        )
        .map_err(|error| error.to_string())?;
    let signals = statement
        .query_map([task_id], |row| {
            Ok(SignalRow {
                kind: row.get(0)?,
                source: row.get(1)?,
                value: row.get(2)?,
                confidence: row.get(3)?,
                time: row.get(4)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut score = TaskScore {
        id: task_id.to_string(),
        title: String::new(),
        state: String::new(),
        weight: 1.0,
        score: 0.0,
        freshness: 0.0,
        verified: false,
        covered: false,
        unknown: true,
        kind: String::new(),
        source: String::new(),
        time: 0,
    };
    // The user's verdict is final; otherwise the newest signal decides.
    let verdict = signals
        .iter()
        .find(|signal| signal.kind == "verdict" && signal.source == "user");
    let Some(chosen) = verdict.or_else(|| signals.first()) else {
        return Ok(score);
    };
    let age_days = at.saturating_sub(chosen.time) as f64 / 86_400_000.0;
    let value = match chosen.kind.as_str() {
        "verdict" | "estimate" => chosen.value.unwrap_or(0.0),
        "test_pass" | "test_fail" | "commit" | "checkpoint" | "git_commit" => {
            chosen.value.unwrap_or(1.0)
        }
        // Legacy uncorroborated claim: half its confidence, halved after a week.
        "assistant_claim" => {
            let decay = if age_days > 7.0 { 0.5 } else { 1.0 };
            0.5 * chosen.confidence.clamp(0.0, 1.0) * decay
        }
        "touched" => 0.5 * chosen.confidence.clamp(0.0, 1.0),
        _ => chosen.value.unwrap_or(0.0),
    };
    score.score = value.clamp(0.0, 1.0);
    score.freshness = freshness_at(at, chosen.time);
    score.verified = verified_kind(&chosen.kind, &chosen.source);
    score.covered = true;
    score.unknown = false;
    score.kind = chosen.kind.clone();
    score.source = chosen.source.clone();
    score.time = chosen.time;
    Ok(score)
}

/// A stated completion for the whole project (no aspect) or for one aspect:
/// what a harness model, the detector, or the user reported, with evidence.
struct Estimate {
    id: i64,
    value: f64,
    confidence: f64,
    source: String,
    evidence: String,
    time: u64,
}

impl Estimate {
    /// How much this estimate is worth today: its confidence, faded by age.
    fn strength(&self, at: u64) -> f64 {
        self.confidence.clamp(0.0, 1.0) * freshness_at(at, self.time)
    }

    fn json(&self) -> Value {
        json!({
            "signal": self.id,
            "percent": percent(self.value),
            "confidence": self.confidence,
            "source": self.source,
            "evidence": self.evidence,
            "time": self.time,
        })
    }
}

type EstimateRow = (Option<String>, Estimate);

fn estimate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EstimateRow> {
    Ok((
        row.get(1)?,
        Estimate {
            id: row.get(0)?,
            value: row.get::<_, f64>(2)?.clamp(0.0, 1.0),
            confidence: row.get(3)?,
            source: row.get(4)?,
            evidence: row.get(5)?,
            time: row.get(6)?,
        },
    ))
}

type Estimates = (Option<Estimate>, HashMap<String, Estimate>, Vec<Value>);

/// The newest whole-project estimate, the newest estimate of each aspect, and
/// the recent whole-project history (oldest first) for the trend line.
fn load_estimates(db: &Connection, project_id: &str) -> Result<Estimates> {
    let mut statement = db
        .prepare(
            "SELECT id,aspect_id,value,confidence,source,evidence,time FROM signals
             WHERE project_id=?1 AND kind='estimate' AND task_id IS NULL
               AND aspect_id IS NULL AND value IS NOT NULL
             ORDER BY time DESC, id DESC LIMIT 24",
        )
        .map_err(|error| error.to_string())?;
    let history = statement
        .query_map([project_id], estimate_row)
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let trend: Vec<Value> = history
        .iter()
        .rev()
        .map(|(_, estimate)| {
            json!({"time":estimate.time,"percent":percent(estimate.value),"source":estimate.source})
        })
        .collect();
    let project = history.into_iter().next().map(|(_, estimate)| estimate);
    let mut statement = db
        .prepare(
            "SELECT id,aspect_id,value,confidence,source,evidence,time FROM signals
             WHERE id IN (SELECT MAX(id) FROM signals
                 WHERE project_id=?1 AND kind='estimate' AND task_id IS NULL
                   AND aspect_id IS NOT NULL AND value IS NOT NULL
                 GROUP BY aspect_id)",
        )
        .map_err(|error| error.to_string())?;
    let aspects = statement
        .query_map([project_id], estimate_row)
        .map_err(|error| error.to_string())?
        .filter_map(|row| row.ok())
        .filter_map(|(aspect, estimate)| aspect.map(|aspect| (aspect, estimate)))
        .collect();
    Ok((project, aspects, trend))
}

/// Weighted mean of one per-task quantity; zero weights fall back to a plain
/// mean so a part never divides by zero.
fn weighted(tasks: &[TaskScore], pick: impl Fn(&TaskScore) -> f64) -> f64 {
    if tasks.is_empty() {
        return 0.0;
    }
    let total: f64 = tasks.iter().map(|task| task.weight).sum();
    if total > 0.0 {
        tasks.iter().map(|task| pick(task) * task.weight).sum::<f64>() / total
    } else {
        tasks.iter().map(&pick).sum::<f64>() / tasks.len() as f64
    }
}

fn task_item(task: &TaskScore) -> Value {
    json!({
        "id": task.id,
        "title": task.title,
        "state": task.state,
        "weight": task.weight,
        "percent": if task.unknown { Value::Null } else { json!(percent(task.score)) },
        "verified": task.verified,
        "kind": task.kind,
        "source": task.source,
        "time": if task.unknown { Value::Null } else { json!(task.time) },
    })
}

fn percent(value: f64) -> f64 {
    (value * 1000.0).round() / 10.0
}

/// Content hash of one captured pane (BUG-087). FNV-1a over the exact bytes:
/// stable across restarts, unlike a `DefaultHasher`, so an unchanged screen
/// keeps one row instead of a new one after every daemon boot.
pub fn tail_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Screens kept per session. A tail is evidence, not a transcript: two hundred
/// distinct screens is more history than a review can use and still bounded.
const TAIL_ROWS_PER_SESSION: i64 = 200;

/// One aspect's contribution to the headline.
struct Part {
    weight: f64,
    value: Option<f64>,
    confidence: f64,
    time: u64,
    estimated: bool,
}

/// Completion of one project at time `at`.
///
/// Each aspect takes the latest estimate a harness model, the detector, or the
/// user stated for it, unless verified task evidence is newer; then (or with no
/// estimate) it is the weighted mean of its tasks. The headline is the newest of the latest whole-project estimate and
/// the weighted mean of the aspects; an aspect with no value of its own uses
/// the whole-project estimate as a stand-in. Nothing at all is `null`, never 0.
fn graph_score_at(db: &Connection, project_id: &str, at: u64) -> Result<Value> {
    let mut aspect_statement = db
        .prepare(
            "SELECT id,key,title,kind,weight FROM aspects WHERE project_id=?1 ORDER BY created",
        )
        .map_err(|error| error.to_string())?;
    let aspects = aspect_statement
        .query_map([project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(aspect_statement);

    let mut task_statement = db
        .prepare(
            "SELECT id,aspect_id,title,state,weight FROM tasks WHERE project_id=?1
             ORDER BY created, id",
        )
        .map_err(|error| error.to_string())?;
    let tasks = task_statement
        .query_map([project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(task_statement);

    let (project_estimate, aspect_estimates, trend) = load_estimates(db, project_id)?;

    let mut grouped: HashMap<String, Vec<TaskScore>> = HashMap::new();
    let mut counts = json!({"total":0,"todo":0,"in_progress":0,"done":0,"blocked":0,"dropped":0});
    let mut unassigned = Vec::new();
    let mut total_weight = 0.0;
    let mut verified_weight = 0.0;
    let mut covered_tasks = 0;
    let mut total_tasks = 0;
    let mut freshness_sum = 0.0;
    let mut unknown_tasks = 0;
    for (id, aspect_id, title, state, weight) in tasks {
        if state == "dropped" {
            if let Some(count) = counts["dropped"].as_u64() {
                counts["dropped"] = json!(count + 1);
            }
            continue;
        }
        total_tasks += 1;
        total_weight += weight.max(0.0);
        if let Some(count) = counts["total"].as_u64() {
            counts["total"] = json!(count + 1);
        }
        let key = match state.as_str() {
            "done" => "done",
            "blocked" => "blocked",
            "in_progress" => "in_progress",
            _ => "todo",
        };
        if let Some(count) = counts[key].as_u64() {
            counts[key] = json!(count + 1);
        }
        let mut score = score_task(db, &id, at)?;
        score.title = title;
        score.state = state;
        score.weight = weight.max(0.0);
        let Some(aspect) = aspect_id else {
            unassigned.push(task_item(&score));
            continue;
        };
        if score.unknown {
            unknown_tasks += 1;
        }
        if score.covered {
            covered_tasks += 1;
            freshness_sum += score.freshness;
            if score.verified {
                verified_weight += score.weight;
            }
        }
        grouped.entry(aspect).or_default().push(score);
    }

    let mut aspect_values = Vec::new();
    let mut parts = Vec::new();
    let mut stale_parts = 0;
    let has_aspects = !aspects.is_empty();
    for (id, key, title, kind, weight) in aspects {
        let scores = grouped.remove(&id).unwrap_or_default();
        let count = scores.len();
        let derived = (count > 0).then(|| weighted(&scores, |task| task.score));
        let verified = weighted(&scores, |task| if task.verified { 1.0 } else { 0.0 });
        let covered = scores.iter().filter(|task| task.covered).count();
        let task_confidence = if count == 0 {
            0.0
        } else {
            let freshness = scores.iter().map(|task| task.freshness).sum::<f64>() / count as f64;
            freshness * covered as f64 / count as f64
        };
        let derived_time = scores
            .iter()
            .filter(|task| task.covered)
            .map(|task| task.time)
            .max();
        // Verified task evidence (a user verdict, a test, a commit) is what may
        // overrule a stated estimate; a model ticking one task may not, or a
        // single tracked task would replace its own judgment of the part.
        let verified_time = scores
            .iter()
            .filter(|task| task.covered && task.verified)
            .map(|task| task.time)
            .max();
        let estimate = aspect_estimates.get(&id);
        let use_estimate = estimate.is_some_and(|estimate| {
            derived.is_none() || verified_time.is_none_or(|time| estimate.time >= time)
        });
        let (value, basis, time, confidence) = match (estimate, derived) {
            (Some(estimate), _) if use_estimate => (
                Some(estimate.value),
                "estimate",
                Some(estimate.time),
                estimate.strength(at),
            ),
            (_, Some(derived)) => (Some(derived), "tasks", derived_time, task_confidence),
            _ => (None, "none", None, 0.0),
        };
        if time.is_some_and(|time| at.saturating_sub(time) > 14 * 86_400_000) {
            stale_parts += 1;
        }
        parts.push(Part {
            weight: weight.max(0.0),
            value,
            confidence,
            time: time.unwrap_or(0),
            estimated: basis == "estimate",
        });
        aspect_values.push(json!({
            "id": id,
            "key": key,
            "title": title,
            "kind": kind,
            "weight": weight,
            "percent": value.map(percent),
            "percent_null": value.is_none(),
            "basis": basis,
            "estimate": estimate.map(Estimate::json),
            "derived": derived.map(percent),
            "time": time,
            "verified": percent(verified),
            "confidence": round3(confidence),
            "tasks": count,
            "unknown": scores.iter().filter(|task| task.unknown).count(),
            "items": scores.iter().map(task_item).collect::<Vec<_>>(),
        }));
    }

    let newest_part = parts
        .iter()
        .filter(|part| part.value.is_some())
        .map(|part| part.time)
        .max();
    let stand_in = project_estimate
        .as_ref()
        .map(|estimate| (estimate.value, estimate.strength(at)));
    let mut sum = 0.0;
    let mut weight_sum = 0.0;
    let mut confidence_sum = 0.0;
    let mut contributing = 0;
    let mut standing_in = 0;
    let mut uses_estimates = false;
    if newest_part.is_some() {
        for part in parts.iter().filter(|part| part.weight > 0.0) {
            let (value, confidence) = match (part.value, stand_in) {
                (Some(value), _) => (value, part.confidence),
                (None, Some(stand_in)) => {
                    standing_in += 1;
                    stand_in
                }
                (None, None) => continue,
            };
            uses_estimates |= part.estimated;
            contributing += 1;
            sum += value * part.weight;
            weight_sum += part.weight;
            confidence_sum += confidence * part.weight;
        }
    }
    uses_estimates |= standing_in > 0;
    // Task-only projects keep the original measure: coverage × freshness.
    let task_confidence = {
        let coverage = if total_tasks > 0 {
            covered_tasks as f64 / total_tasks as f64
        } else {
            0.0
        };
        let freshness = if covered_tasks > 0 {
            freshness_sum / covered_tasks as f64
        } else {
            0.0
        };
        (coverage * freshness).clamp(0.0, 1.0)
    };
    let estimate_leads = project_estimate.as_ref().is_some_and(|estimate| {
        weight_sum <= 0.0 || newest_part.is_none_or(|time| estimate.time >= time)
    });
    let (headline, confidence, basis) = match &project_estimate {
        Some(estimate) if estimate_leads => {
            (Some(estimate.value), estimate.strength(at), "estimate")
        }
        _ if weight_sum > 0.0 => (
            Some(sum / weight_sum),
            if uses_estimates {
                confidence_sum / weight_sum
            } else {
                task_confidence
            },
            "parts",
        ),
        _ => (None, 0.0, "none"),
    };
    let verified = if total_weight > 0.0 {
        percent(verified_weight / total_weight)
    } else {
        0.0
    };

    let mut risks = Vec::new();
    if !has_aspects && project_estimate.is_none() {
        risks.push("No aspects defined".to_string());
    }
    if let Some(estimate) = &project_estimate
        && basis == "estimate"
        && at.saturating_sub(estimate.time) > 7 * 86_400_000
    {
        risks.push(format!(
            "The newest estimate is {} days old",
            at.saturating_sub(estimate.time) / 86_400_000
        ));
    }
    if stale_parts > 0 {
        risks.push(format!(
            "{} no evidence from the last two weeks",
            counted_have(stale_parts as u64, "part")
        ));
    }
    // Work committed after the number was set is work the number has not seen.
    let headline_time = match (basis, &project_estimate) {
        ("estimate", Some(estimate)) => Some(estimate.time),
        ("parts", _) => newest_part,
        _ => None,
    };
    let commits_since: i64 = match headline_time {
        Some(time) => db
            .query_row(
                "SELECT COUNT(*) FROM signals WHERE project_id=?1 AND kind='git_commit'
                   AND time>?2 AND time<=?3",
                params![project_id, time as i64, at as i64],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?,
        None => 0,
    };
    if commits_since > 0 {
        risks.push(format!(
            "{commits_since} commit{} since the newest estimate; reassess to count {}",
            if commits_since == 1 { "" } else { "s" },
            if commits_since == 1 { "it" } else { "them" }
        ));
    }
    if unknown_tasks > 0 {
        risks.push(format!("{} no evidence", counted_have(unknown_tasks as u64, "task")));
    }
    if !unassigned.is_empty() {
        risks.push(format!("{} no part", counted_have(unassigned.len() as u64, "task")));
    }
    if counts["blocked"].as_u64().unwrap_or(0) > 0 {
        let blocked = counts["blocked"].as_u64().unwrap_or(0);
        risks.push(format!("{} {} blocked", counted(blocked, "task"), if blocked == 1 { "is" } else { "are" }));
    }
    let (why, basis_value) = match (basis, &project_estimate) {
        ("estimate", Some(estimate)) => (
            format!(
                "Whole-project estimate from {}: {}",
                source_name(&estimate.source),
                estimate.evidence
            ),
            json!({"kind":"estimate","source":estimate.source,"time":estimate.time,
                "evidence":estimate.evidence,"confidence":estimate.confidence}),
        ),
        ("parts", _) => {
            let mut text = format!(
                "Weighted mean of {}; {covered_tasks} of {} {}",
                counted(contributing as u64, "part"),
                counted(total_tasks as u64, "task"),
                if total_tasks == 1 { "carries evidence" } else { "carry evidence" }
            );
            if standing_in > 0 {
                text.push_str(&format!(
                    "; {} without a value {} the whole-project estimate",
                    counted(standing_in as u64, "part"),
                    if standing_in == 1 { "uses" } else { "use" }
                ));
            }
            (
                text,
                json!({"kind":"parts","parts":contributing,"standing_in":standing_in,
                    "time":newest_part}),
            )
        }
        _ => (
            "No estimate or evidence yet".to_string(),
            json!({"kind":"none"}),
        ),
    };
    // Every number is derived from these rows, so the payload carries them:
    // a percentage whose evidence cannot be shown is a bug (BUG-084/089).
    // Working-tree flips (git_dirty/git_clean) feed no number and arrive with
    // every save during active work; only the newest, the current state, is
    // listed, so they do not push estimates out of the 40 rows.
    let mut signal_statement = db
        .prepare(
            "SELECT id,task_id,aspect_id,kind,source,value,confidence,evidence,time
             FROM signals WHERE project_id=?1
               AND (kind NOT IN ('git_dirty','git_clean') OR id=(SELECT MAX(id) FROM signals
                    WHERE project_id=?1 AND kind IN ('git_dirty','git_clean')))
             ORDER BY time DESC, id DESC LIMIT 40",
        )
        .map_err(|error| error.to_string())?;
    let signals = signal_statement
        .query_map([project_id], |row| {
            let kind: String = row.get(3)?;
            let source: String = row.get(4)?;
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "task_id": row.get::<_, Option<String>>(1)?,
                "aspect_id": row.get::<_, Option<String>>(2)?,
                "verified": verified_kind(&kind, &source),
                "assistant": source == "assistant",
                "kind": kind,
                "source": source,
                "value": row.get::<_, Option<f64>>(5)?,
                "confidence": row.get::<_, f64>(6)?,
                "evidence": row.get::<_, String>(7)?,
                "time": row.get::<_, u64>(8)?,
            }))
        })
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(signal_statement);
    let signal_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE project_id=?1",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let project = fetch_project(db, project_id)?;
    Ok(json!({
        "project": project,
        "percent": headline.map(percent),
        "percent_null": headline.is_none(),
        "verified": verified,
        "confidence": round3(confidence),
        "as_of": at,
        "why": why,
        "basis": basis_value,
        "estimate": project_estimate.as_ref().map(Estimate::json),
        "trend": trend,
        "aspects": aspect_values,
        "unassigned": unassigned,
        "tasks": counts,
        "risks": risks,
        "commits_since": commits_since,
        "signals": signals,
        "signal_count": signal_count,
    }))
}

/// One line per project for a spoken answer or a harness tool result.
pub fn progress_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("This project");
    let Some(value) = graph["percent"].as_f64() else {
        return format!("{name} has no completion estimate yet.");
    };
    // "About" hedges an estimate; 0% and 100% are exact.
    let hedge = if (0.5..99.5).contains(&value) { "about " } else { "" };
    let mut sentence = format!("{name} is {hedge}{}% complete", value.round());
    match graph["basis"]["kind"].as_str() {
        Some("estimate") => {
            match graph["basis"]["source"].as_str().unwrap_or("assistant") {
                "user" => sentence.push_str(" by your estimate"),
                source => sentence.push_str(&format!(" by {}'s estimate", source_name(source))),
            }
        }
        Some("parts") => match graph["aspects"].as_array().map(Vec::as_slice) {
            Some([only]) => sentence.push_str(&format!(
                ". Its only part is {}",
                only["title"].as_str().unwrap_or("unnamed")
            )),
            _ => sentence.push_str(" across its parts"),
        },
        _ => {}
    }
    sentence.push('.');
    let weakest = graph["aspects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|aspect| Some((aspect["title"].as_str()?, aspect["percent"].as_f64()?)))
        .min_by(|left, right| left.1.total_cmp(&right.1));
    if let Some((title, value)) = weakest
        && graph["aspects"].as_array().is_some_and(|aspects| aspects.len() > 1)
    {
        sentence.push_str(&format!(" Furthest behind: {title} at {}%.", value.round()));
    }
    match graph["commits_since"].as_i64().unwrap_or(0) {
        0 => {}
        1 => sentence.push_str(" 1 commit landed since the last estimate."),
        commits => sentence.push_str(&format!(
            " {commits} commits landed since the last estimate."
        )),
    }
    sentence
}

/// One part, spoken: "Voice is at 100%, with 2 of 2 tasks done. The project
/// overall is at 87%."
pub fn part_sentence(graph: &Value, aspect: &Value) -> String {
    let title = aspect["title"].as_str().unwrap_or("That part");
    let items = aspect["items"].as_array().cloned().unwrap_or_default();
    let done = items.iter().filter(|item| item["state"] == "done").count();
    let tasks = match (done, items.len()) {
        (_, 0) => String::new(),
        (0, 1) => "1 open task".to_string(),
        (0, open) => format!("{open} open tasks"),
        (done, total) => format!("{done} of {total} tasks done"),
    };
    let mut sentence = match aspect["percent"].as_f64() {
        None => format!("{title} has no estimate yet"),
        Some(value) => {
            let mut text = format!("{title} is at {}%", value.round());
            if aspect["basis"] == "estimate" {
                match aspect["estimate"]["source"].as_str().unwrap_or("assistant") {
                    "user" => text.push_str(" by your estimate"),
                    source => text.push_str(&format!(" by {}'s estimate", source_name(source))),
                }
            }
            text
        }
    };
    if !tasks.is_empty() {
        sentence.push_str(&format!(", with {tasks}"));
    }
    sentence.push('.');
    if let Some(value) = graph["percent"].as_f64() {
        sentence.push_str(&format!(" The project overall is at {}%.", value.round()));
    }
    sentence
}

/// "What changed today": the estimates, task updates and commits recorded
/// since `since` (from the graph's newest signals), then the headline.
pub fn recent_sentence(graph: &Value, since: u64) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("this project");
    let signals: Vec<&Value> = graph["signals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|signal| signal["time"].as_u64().is_some_and(|time| time >= since))
        .collect();
    let titles: BTreeMap<&str, &str> = graph["aspects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|aspect| Some((aspect["id"].as_str()?, aspect["title"].as_str()?)))
        .collect();
    // Newest estimate per target, newest first.
    let mut seen = BTreeSet::new();
    let mut estimates = Vec::new();
    for signal in signals.iter().filter(|signal| signal["kind"] == "estimate") {
        let target = signal["aspect_id"].as_str();
        if !seen.insert(target) {
            continue;
        }
        let who = match signal["source"].as_str().unwrap_or("assistant") {
            "user" => "you".to_string(),
            source => source_name(source),
        };
        let what = target.and_then(|id| titles.get(id).copied()).unwrap_or("the project");
        let value = signal["value"].as_f64().unwrap_or(0.0) * 100.0;
        estimates.push(format!("{who} put {what} at {}%", value.round()));
    }
    let tasks = signals
        .iter()
        .filter_map(|signal| signal["task_id"].as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let commits = signals.iter().filter(|signal| signal["kind"] == "git_commit").count();
    let mut changes = Vec::new();
    if !estimates.is_empty() {
        let shown = estimates.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
        changes.push(capitalise(&shown));
    }
    if tasks > 0 {
        changes.push(format!("{tasks} task{} updated", if tasks == 1 { "" } else { "s" }));
    }
    if commits > 0 {
        let mut text = format!("{commits} commit{}", if commits == 1 { "" } else { "s" });
        // Signals are newest first; rows from before subjects were recorded have none.
        let latest = signals
            .iter()
            .find(|signal| signal["kind"] == "git_commit")
            .and_then(|signal| signal["evidence"].as_str()?.strip_prefix("HEAD ")?.split_once(' '))
            .map(|(_, subject)| subject.trim())
            .filter(|subject| !subject.is_empty());
        if let Some(subject) = latest {
            text.push_str(&format!(
                ", {} “{subject}”",
                if commits == 1 { "titled" } else { "the latest" }
            ));
        }
        changes.push(text);
    }
    if changes.is_empty() {
        return format!("Nothing was recorded on {name} in the last day.");
    }
    let mut sentence = format!("In the last day on {name}: {}.", changes.join("; "));
    if let Some(value) = graph["percent"].as_f64() {
        sentence.push_str(&format!(" It is now about {}% complete.", value.round()));
    }
    sentence
}

pub fn capitalise(text: &str) -> String {
    let mut characters = text.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}

/// Every task in the graph with the title of its part ("" when unassigned).
fn graph_tasks(graph: &Value) -> Vec<(&str, &Value)> {
    graph["aspects"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|aspect| {
            let title = aspect["title"].as_str().unwrap_or_default();
            aspect["items"].as_array().into_iter().flatten().map(move |item| (title, item))
        })
        .chain(graph["unassigned"].as_array().into_iter().flatten().map(|item| ("", item)))
        .collect()
}

fn task_list(tasks: &[(&str, &Value)], limit: usize) -> String {
    let mut list = tasks
        .iter()
        .take(limit)
        .map(|(part, item)| {
            let title = item["title"].as_str().unwrap_or("Untitled");
            if part.is_empty() { format!("“{title}”") } else { format!("“{title}” in {part}") }
        })
        .collect::<Vec<_>>()
        .join(", ");
    if tasks.len() > limit {
        list.push_str(&format!(" and {} more", tasks.len() - limit));
    }
    list
}

/// "What's blocked": the blocked tasks, named with their parts.
pub fn blocked_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("this project");
    let blocked: Vec<(&str, &Value)> =
        graph_tasks(graph).into_iter().filter(|(_, item)| item["state"] == "blocked").collect();
    match blocked.len() {
        0 => format!("Nothing is marked blocked in {name}."),
        1 => format!("1 task is blocked: {}.", task_list(&blocked, 5)),
        count => format!("{count} tasks are blocked: {}.", task_list(&blocked, 5)),
    }
}

/// "What's done": finished parts and the count of finished tasks.
pub fn done_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("This project");
    let finished: Vec<&str> = graph["aspects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|aspect| aspect["percent"].as_f64().is_some_and(|value| value >= 100.0))
        .filter_map(|aspect| aspect["title"].as_str())
        .collect();
    let tasks = graph_tasks(graph);
    let done: Vec<(&str, &Value)> = tasks.iter().copied().filter(|(_, item)| item["state"] == "done").collect();
    let mut sentence = match finished.as_slice() {
        [] => format!("No part of {name} is at 100% yet."),
        [one] => format!("{one} is finished."),
        many => format!("Finished parts: {}.", many.join(", ")),
    };
    if !tasks.is_empty() {
        sentence.push_str(&format!(" {} of {} tasks are done", done.len(), tasks.len()));
        if !done.is_empty() && done.len() <= 5 {
            sentence.push_str(&format!(": {}", task_list(&done, 5)));
        }
        sentence.push('.');
    }
    sentence
}

/// "What's left": the headline, every unfinished part from furthest behind,
/// and up to five open tasks.
pub fn remaining_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("This project");
    let Some(value) = graph["percent"].as_f64() else {
        return format!("{name} has no completion estimate yet.");
    };
    let aspects = graph["aspects"].as_array().cloned().unwrap_or_default();
    let mut parts: Vec<(&str, f64)> = aspects
        .iter()
        .filter_map(|aspect| Some((aspect["title"].as_str()?, aspect["percent"].as_f64()?)))
        .filter(|(_, percent)| *percent < 100.0)
        .collect();
    parts.sort_by(|left, right| left.1.total_cmp(&right.1));
    let open: Vec<&str> = aspects
        .iter()
        .flat_map(|aspect| aspect["items"].as_array().into_iter().flatten())
        .chain(graph["unassigned"].as_array().into_iter().flatten())
        .filter(|item| item["state"] != "done")
        .filter_map(|item| item["title"].as_str())
        .collect();
    let hedge = if (0.5..99.5).contains(&value) { "about " } else { "" };
    let mut sentence = format!("{name} is {hedge}{}% complete.", value.round());
    if parts.is_empty() && open.is_empty() {
        sentence.push_str(" Every part is at 100% and no task is open.");
        return sentence;
    }
    if !parts.is_empty() {
        let list = parts
            .iter()
            .map(|(title, percent)| format!("{title} {}%", percent.round()))
            .collect::<Vec<_>>()
            .join(", ");
        sentence.push_str(&format!(" Unfinished: {list}."));
    }
    if !open.is_empty() {
        let shown = open.iter().take(5).copied().collect::<Vec<_>>().join(", ");
        let more = open.len().saturating_sub(5);
        sentence.push_str(&format!(" Open tasks: {shown}"));
        if more > 0 {
            sentence.push_str(&format!(" and {more} more"));
        }
        sentence.push('.');
    }
    sentence
}

/// Full plain-text view of a graph for a harness tool result.
pub fn progress_text(graph: &Value) -> String {
    let mut lines = vec![progress_sentence(graph)];
    if let Some(why) = graph["why"].as_str() {
        lines.push(format!("Basis: {why}"));
    }
    if graph["pace"]["days"].as_f64().unwrap_or(0.0) >= 1.0 {
        lines.push(format!("Pace: {}", pace_sentence(graph)));
    }
    let aspects = graph["aspects"].as_array().cloned().unwrap_or_default();
    if aspects.is_empty() {
        lines.push("No parts are defined yet. Report a part by name to create it.".into());
    } else {
        lines.push("Parts:".into());
    }
    for aspect in &aspects {
        let title = aspect["title"].as_str().unwrap_or("part");
        let value = aspect["percent"]
            .as_f64()
            .map(|value| format!("{}%", value.round()))
            .unwrap_or_else(|| "unknown".into());
        let basis = match aspect["basis"].as_str() {
            Some("estimate") => format!(
                "estimate by {}",
                source_name(aspect["estimate"]["source"].as_str().unwrap_or("assistant"))
            ),
            Some("tasks") => counted(aspect["tasks"].as_u64().unwrap_or(0), "task"),
            _ => "no evidence".into(),
        };
        lines.push(format!("- {title}: {value} ({basis})"));
        for item in aspect["items"].as_array().into_iter().flatten().take(20) {
            let mark = match item["state"].as_str() {
                Some("done") => "x",
                Some("in_progress") => "~",
                Some("blocked") => "!",
                _ => " ",
            };
            lines.push(format!(
                "  - [{mark}] {}",
                item["title"].as_str().unwrap_or("task")
            ));
        }
    }
    for item in graph["unassigned"].as_array().into_iter().flatten().take(20) {
        lines.push(format!(
            "- (no part) [{}] {}",
            if item["state"] == "done" { "x" } else { " " },
            item["title"].as_str().unwrap_or("task")
        ));
    }
    // What the newest estimate has not seen, by subject, so a harness can
    // judge which parts moved (newest first; rows recorded with a subject).
    let since = graph["basis"]["time"].as_u64().unwrap_or(u64::MAX);
    let subjects: Vec<&str> = graph["signals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|signal| signal["kind"] == "git_commit")
        .filter(|signal| signal["time"].as_u64().is_some_and(|time| time > since))
        .filter_map(|signal| signal["evidence"].as_str()?.strip_prefix("HEAD ")?.split_once(' '))
        .map(|(_, subject)| subject.trim())
        .filter(|subject| !subject.is_empty())
        .take(5)
        .collect();
    if !subjects.is_empty() {
        // One per line: a subject may itself contain "; ".
        lines.push("Recent commits:".to_string());
        lines.extend(subjects.iter().map(|subject| format!("- {subject}")));
    }
    for risk in graph["risks"].as_array().into_iter().flatten() {
        if let Some(risk) = risk.as_str() {
            lines.push(format!("Risk: {risk}"));
        }
    }
    lines.join("\n")
}

/// Display name for a signal source: harness ids read as product names.
pub fn source_name(source: &str) -> String {
    match source {
        "claude" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "pi" => "Pi".into(),
        "opencode" => "OpenCode".into(),
        "agy" => "Antigravity".into(),
        "navi" => "Navi".into(),
        "detector" => "the detector".into(),
        "user" => "you".into(),
        "assistant" => "the assistant".into(),
        other => other.to_string(),
    }
}

/// A part or task name as a model wrote it: one line, no quotes, bounded.
fn clean_label(text: &str, limit: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

/// Signal sources are short identifiers the UI can badge.
fn source_label(source: Option<&str>) -> String {
    let cleaned: String = source
        .unwrap_or("assistant")
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "assistant".into()
    } else {
        cleaned
    }
}

pub fn graph_score(db: &Connection, project_id: &str) -> Result<Value> {
    let at = now();
    let mut graph = graph_score_at(db, project_id, at)?;
    graph["pace"] = record_pace(db, project_id, graph["percent"].as_f64(), at)?;
    Ok(graph)
}

/// How long a pace looks back.
const PACE_DAYS: u64 = 14;

/// Keeps the headline once per day (the day's last value) and compares it with
/// the oldest kept day of the last two weeks: `per_day` points a day over
/// `days`, and `eta_days` to 100% at that pace while it is rising.
fn record_pace(db: &Connection, project_id: &str, percent: Option<f64>, at: u64) -> Result<Value> {
    let Some(value) = percent else {
        return Ok(json!({"days":0}));
    };
    let day = at / 86_400_000;
    db.execute(
        "INSERT INTO progress_days(project_id,day,percent,time) VALUES(?1,?2,?3,?4)
         ON CONFLICT(project_id,day) DO UPDATE SET percent=excluded.percent,time=excluded.time
         WHERE progress_days.percent<>excluded.percent",
        params![project_id, day as i64, value, at as i64],
    )
    .map_err(|error| error.to_string())?;
    // The last month of daily headlines, oldest first, for the trend line.
    let mut statement = db
        .prepare(
            "SELECT percent,time FROM progress_days WHERE project_id=?1 AND day>=?2 ORDER BY day",
        )
        .map_err(|error| error.to_string())?;
    let history: Vec<Value> = statement
        .query_map(params![project_id, day.saturating_sub(30) as i64], |row| {
            Ok(json!({"percent":row.get::<_, f64>(0)?,"time":row.get::<_, i64>(1)?}))
        })
        .map_err(|error| error.to_string())?
        .filter_map(|row| row.ok())
        .collect();
    drop(statement);
    let oldest: Option<(f64, i64)> = db
        .query_row(
            "SELECT percent,time FROM progress_days WHERE project_id=?1 AND day>=?2 AND day<?3
             ORDER BY day LIMIT 1",
            params![project_id, day.saturating_sub(PACE_DAYS) as i64, day as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((before, since)) = oldest else {
        return Ok(json!({"days":0,"history":history}));
    };
    let days = (at.saturating_sub(since as u64) as f64 / 86_400_000.0).max(1.0);
    let per_day = (value - before) / days;
    // Under a twentieth of a point a day is standing still, not a date.
    let eta = (per_day >= 0.05 && value < 100.0).then(|| ((100.0 - value) / per_day).ceil());
    Ok(json!({"days":days.round(),"from":before,"since":since,"per_day":(per_day * 10.0).round() / 10.0,
        "eta_days":eta,"history":history}))
}

/// "How long until we're done": the pace of the last two weeks and the day
/// 100% falls on at that pace.
pub fn pace_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("The project");
    let Some(value) = graph["percent"].as_f64() else {
        return format!("{name} has no estimate yet, so there is no pace to project.");
    };
    if value >= 99.5 {
        return format!("{name} is already at 100%.");
    }
    let pace = &graph["pace"];
    let days = pace["days"].as_f64().unwrap_or(0.0);
    if days < 1.0 {
        return format!(
            "{name} is at {}%. I record the headline once a day, so a pace needs a day of history. Ask again tomorrow.",
            value.round()
        );
    }
    let from = pace["from"].as_f64().unwrap_or(value);
    let span = if days == 1.0 { "the last day".to_string() } else { format!("the last {days} days") };
    let moved = if from.round() == value.round() {
        format!("{name} has stayed at {}% for {span}", value.round())
    } else {
        format!("{name} went from {}% to {}% in {span}", from.round(), value.round())
    };
    match pace["eta_days"].as_f64() {
        Some(eta) => {
            let when = match eta {
                eta if eta <= 1.0 => "about a day".to_string(),
                eta if eta <= 21.0 => format!("about {eta} days"),
                eta if eta <= 120.0 => format!("about {} weeks", (eta / 7.0).round()),
                eta if eta <= 365.0 => format!("about {} months", (eta / 30.0).round()),
                _ => "more than a year".to_string(),
            };
            format!(
                "{moved}, about {} points a day. At that pace it reaches 100% in {when}.",
                pace["per_day"].as_f64().unwrap_or(0.0)
            )
        }
        None if value < from => format!("{moved}, so it is moving backwards and there is no finish date to project."),
        None => format!("{moved}, so there is no finish date to project yet."),
    }
}

impl App {
    pub fn projects(&self) -> Result<Vec<Value>> {
        let state = self.state.lock().unwrap();
        let mut statement = state
            .db
            .prepare(&format!(
                "SELECT {PROJECT_COLUMNS} FROM projects WHERE archived=0 ORDER BY created"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], project_row)
            .map_err(|error| error.to_string())?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn resolve_project(&self, reference: &str) -> Result<Value> {
        let state = self.state.lock().unwrap();
        fetch_project(&state.db, reference)
    }

    pub fn project_by_id(&self, project_id: &str) -> Result<Value> {
        let state = self.state.lock().unwrap();
        fetch_project(&state.db, project_id)
    }

    pub fn add_project(&self, name: &str, path: &str) -> Result<Value> {
        let name = name.trim();
        if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
            return Err("Invalid project name".into());
        }
        if path.trim().is_empty() {
            return Err("Missing project path".into());
        }
        let canonical = fs::canonicalize(path.trim())
            .map_err(|error| format!("Invalid project directory: {error}"))?;
        if !canonical.is_dir() {
            return Err("Project path must be a directory".into());
        }
        let canonical = canonical.to_string_lossy().into_owned();
        let existing = {
            let state = self.state.lock().unwrap();
            let by_path: Option<String> = state
                .db
                .query_row("SELECT id FROM projects WHERE path=?1", [&canonical], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(|error| error.to_string())?;
            if by_path.is_some() {
                return Err("This directory is already a project".into());
            }
            let count: i64 = state
                .db
                .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if count >= 200 {
                return Err("Too many projects".into());
            }
            let name_taken: Option<i64> = state
                .db
                .query_row(
                    "SELECT 1 FROM projects WHERE lower(name)=lower(?1)",
                    [name],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if name_taken.is_some() {
                return Err("A project with this name already exists".into());
            }
            let id = format!("prj-{}", &id()[..8]);
            state
                .db
                .execute(
                    "INSERT INTO projects(id,name,path,origin,path_state,archived,created,git_dirty)
                     VALUES(?1,?2,?3,'manual','ok',0,?4,0)",
                    params![id, name, canonical, now()],
                )
                .map_err(|error| error.to_string())?;
            id
        };
        let value = self.project_by_id(&existing)?;
        let _ = self.probe_project(&existing);
        self.event_for(
            Some(&existing),
            None,
            "project",
            "Project added",
            json!({"name":name,"path":value["path"]}),
        );
        Ok(value)
    }

    /// The HEAD the probes last saw at or before `time`, from the
    /// `git_commit` signals ("HEAD <hash> <subject>").
    pub fn head_at(&self, project_id: &str, time: u64) -> Option<String> {
        let state = self.state.lock().unwrap();
        state
            .db
            .query_row(
                "SELECT evidence FROM signals WHERE project_id=?1 AND kind='git_commit'
                   AND time<=?2 ORDER BY time DESC, id DESC LIMIT 1",
                params![project_id, time as i64],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|evidence| {
                let rest = evidence.strip_prefix("HEAD ")?;
                rest.split_whitespace().next().map(str::to_string)
            })
    }

    pub fn probe_project(&self, project_id: &str) -> Result<Value> {
        let project = self.project_by_id(project_id)?;
        let path = project["path"].as_str().unwrap_or_default().to_string();
        let previous_head = project["git_head"].as_str().unwrap_or_default().to_string();
        let previous_dirty = project["git_dirty"].as_bool().unwrap_or(false);
        let (root, head, dirty) = if Path::new(&path).is_dir() {
            git_state(&path)
        } else {
            (None, None, false)
        };
        let state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "UPDATE projects SET git_root=?1,git_head=?2,git_dirty=?3,path_state=?4 WHERE id=?5",
                params![
                    root,
                    head,
                    if dirty { 1 } else { 0 },
                    if Path::new(&path).is_dir() { "ok" } else { "missing" },
                    project_id
                ],
            )
            .map_err(|error| error.to_string())?;
        drop(state);
        if head.as_deref().unwrap_or_default() != previous_head {
            // "HEAD 1a2b3c4d5e6f feat: subject": head_at reads the hash back.
            let evidence = match head.as_deref() {
                Some(head) => {
                    let subject = git(&path, &["log", "-1", "--format=%s", head])
                        .map(|text| text.trim().chars().take(120).collect::<String>())
                        .unwrap_or_default();
                    format!("HEAD {} {subject}", &head[..head.len().min(12)]).trim_end().to_string()
                }
                None => "no commits".to_string(),
            };
            let _ = self.add_signal(
                project_id,
                None,
                None,
                "git_commit",
                "git",
                Some(if head.is_some() { 1.0 } else { 0.0 }),
                1.0,
                &evidence,
            );
        }
        if dirty != previous_dirty {
            let _ = self.add_signal(
                project_id,
                None,
                None,
                if dirty { "git_dirty" } else { "git_clean" },
                "git",
                Some(1.0),
                0.8,
                if dirty {
                    "working tree has changes"
                } else {
                    "working tree is clean"
                },
            );
        }
        self.project_by_id(project_id)
    }

    pub fn graph_get(&self, project_id: Option<&str>) -> Result<Value> {
        let resolved = match project_id.filter(|value| !value.trim().is_empty()) {
            Some(reference) => self.resolve_project(reference)?,
            None => {
                let projects = self.projects()?;
                if projects.len() != 1 {
                    return Err("Select a project".into());
                }
                projects[0].clone()
            }
        };
        let id = resolved["id"].as_str().ok_or("Missing project id")?;
        let state = self.state.lock().unwrap();
        graph_score(&state.db, id)
    }

    pub fn add_aspect(&self, p: &Value) -> Result<Value> {
        let project = self.resolve_project(p["project_id"].as_str().ok_or("Missing project")?)?;
        let project_id = project["id"].as_str().ok_or("Missing project id")?;
        let title = p["title"].as_str().unwrap_or_default().trim();
        if title.is_empty() || title.len() > 160 {
            return Err("Invalid aspect title".into());
        }
        let key = p["key"]
            .as_str()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| slug(title));
        let id = format!("asp-{}", &id()[..8]);
        let state = self.state.lock().unwrap();
        // The setup form is the first caller: a duplicate title must read as a
        // sentence, not as a UNIQUE constraint failure (BUG-089).
        let duplicate: Option<String> = state
            .db
            .query_row(
                "SELECT id FROM aspects WHERE project_id=?1 AND key=?2",
                params![project_id, key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if duplicate.is_some() {
            return Err(format!("An aspect named “{title}” already exists"));
        }
        state
            .db
            .execute(
                "INSERT INTO aspects(id,project_id,key,title,kind,weight,created)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    id,
                    project_id,
                    key,
                    title,
                    p["kind"].as_str().unwrap_or("work"),
                    p["weight"].as_f64().unwrap_or(1.0).clamp(0.0, 100.0),
                    now()
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({"id":id}))
    }

    pub fn set_aspect(&self, p: &Value) -> Result<Value> {
        let id = p["id"].as_str().ok_or("Missing aspect")?;
        let state = self.state.lock().unwrap();
        if let Some(title) = p["title"].as_str() {
            if title.trim().is_empty() {
                return Err("Invalid aspect title".into());
            }
            state
                .db
                .execute(
                    "UPDATE aspects SET title=?1 WHERE id=?2",
                    params![title.trim(), id],
                )
                .map_err(|error| error.to_string())?;
        }
        if let Some(weight) = p["weight"].as_f64() {
            state
                .db
                .execute(
                    "UPDATE aspects SET weight=?1 WHERE id=?2",
                    params![weight.clamp(0.0, 100.0), id],
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(json!({"id":id}))
    }

    pub fn add_task(&self, p: &Value) -> Result<Value> {
        let project = self.resolve_project(p["project_id"].as_str().ok_or("Missing project")?)?;
        let project_id = project["id"].as_str().ok_or("Missing project id")?;
        let title = p["title"].as_str().unwrap_or_default().trim();
        if title.is_empty() || title.len() > 400 {
            return Err("Invalid task title".into());
        }
        let id = format!("tsk-{}", &id()[..8]);
        let state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "INSERT INTO tasks(id,project_id,aspect_id,title,state,weight,created,updated)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
                params![
                    id,
                    project_id,
                    p["aspect_id"].as_str(),
                    title,
                    p["state"].as_str().unwrap_or("todo"),
                    p["weight"].as_f64().unwrap_or(1.0).clamp(0.0, 100.0),
                    now()
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({"id":id}))
    }

    pub fn set_task(&self, p: &Value) -> Result<Value> {
        let id = p["id"].as_str().ok_or("Missing task")?;
        let state = self.state.lock().unwrap();
        let (project_id, old_state): (String, String) = state
            .db
            .query_row(
                "SELECT project_id,state FROM tasks WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| "Unknown task".to_string())?;
        if let Some(title) = p["title"].as_str() {
            if title.trim().is_empty() {
                return Err("Invalid task title".into());
            }
            state
                .db
                .execute(
                    "UPDATE tasks SET title=?1,updated=?2 WHERE id=?3",
                    params![title.trim(), now(), id],
                )
                .map_err(|error| error.to_string())?;
        }
        if let Some(weight) = p["weight"].as_f64() {
            state
                .db
                .execute(
                    "UPDATE tasks SET weight=?1,updated=?2 WHERE id=?3",
                    params![weight.clamp(0.0, 100.0), now(), id],
                )
                .map_err(|error| error.to_string())?;
        }
        if let Some(aspect) = p.get("aspect_id") {
            state
                .db
                .execute(
                    "UPDATE tasks SET aspect_id=?1,updated=?2 WHERE id=?3",
                    params![aspect.as_str(), now(), id],
                )
                .map_err(|error| error.to_string())?;
        }
        if let Some(next_state) = p["state"].as_str() {
            if !matches!(next_state, "todo" | "in_progress" | "done" | "blocked" | "dropped") {
                return Err("Invalid task state".into());
            }
            state
                .db
                .execute(
                    "UPDATE tasks SET state=?1,updated=?2 WHERE id=?3",
                    params![next_state, now(), id],
                )
                .map_err(|error| error.to_string())?;
            if next_state != old_state && next_state != "dropped" {
                insert_signal(
                    &state.db,
                    &project_id,
                    Some(id),
                    p["aspect_id"].as_str(),
                    "verdict",
                    "user",
                    Some(match next_state {
                        "done" => 1.0,
                        "in_progress" => 0.5,
                        _ => 0.0,
                    }),
                    1.0,
                    &format!("task state set to {next_state} by user"),
                )?;
            }
        }
        Ok(json!({"id":id}))
    }

    pub fn add_plan(&self, p: &Value) -> Result<Value> {
        let project = self.resolve_project(p["project_id"].as_str().ok_or("Missing project")?)?;
        let project_id = project["id"].as_str().ok_or("Missing project id")?;
        let title = p["title"].as_str().unwrap_or_default().trim();
        if title.is_empty() {
            return Err("Invalid plan title".into());
        }
        let id = format!("pln-{}", &id()[..8]);
        let state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "INSERT INTO plans(id,project_id,task_id,title,body,state,created,updated)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
                params![
                    id,
                    project_id,
                    p["task_id"].as_str(),
                    title,
                    p["body"].as_str().unwrap_or_default(),
                    p["state"].as_str().unwrap_or("draft"),
                    now()
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({"id":id}))
    }

    pub fn set_plan(&self, p: &Value) -> Result<Value> {
        let id = p["id"].as_str().ok_or("Missing plan")?;
        let state = self.state.lock().unwrap();
        if let Some(body) = p["body"].as_str() {
            state
                .db
                .execute(
                    "UPDATE plans SET body=?1,updated=?2 WHERE id=?3",
                    params![body, now(), id],
                )
                .map_err(|error| error.to_string())?;
        }
        if let Some(status) = p["state"].as_str() {
            state
                .db
                .execute(
                    "UPDATE plans SET state=?1,updated=?2 WHERE id=?3",
                    params![status, now(), id],
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(json!({"id":id}))
    }

    pub fn add_checkpoint(&self, p: &Value) -> Result<Value> {
        let project = self.resolve_project(p["project_id"].as_str().ok_or("Missing project")?)?;
        let project_id = project["id"].as_str().ok_or("Missing project id")?;
        let title = p["title"].as_str().unwrap_or_default().trim();
        let evidence = p["evidence"].as_str().unwrap_or_default().trim();
        if title.is_empty() || evidence.is_empty() {
            return Err("Checkpoint needs a title and evidence".into());
        }
        let id = format!("chk-{}", &id()[..8]);
        let task_id = p["task_id"].as_str();
        let state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "INSERT INTO checkpoints(id,project_id,task_id,title,evidence,created)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, project_id, task_id, title, evidence, now()],
            )
            .map_err(|error| error.to_string())?;
        insert_signal(
            &state.db,
            project_id,
            task_id,
            p["aspect_id"].as_str(),
            "checkpoint",
            "user",
            Some(1.0),
            1.0,
            evidence,
        )?;
        Ok(json!({"id":id}))
    }

    /// Legacy assistant claim: now an `estimate` from the assistant that never
    /// creates parts or tasks.
    pub fn progress_claim(&self, p: &Value) -> Result<Value> {
        let mut params = p.clone();
        params["source"] = json!("assistant");
        params["create"] = json!(false);
        if params["value"].is_null() && params["percent"].is_null() {
            params["value"] = json!(0.5);
        }
        self.progress_report(&params)
    }

    /// The project a progress call is about: an explicit id or name, else the
    /// harness session's project, else the project that contains `cwd`, else
    /// the only project.
    pub fn progress_project(&self, p: &Value) -> Result<String> {
        let explicit = [&p["project_id"], &p["project"]]
            .into_iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .find(|value| !value.is_empty());
        if let Some(reference) = explicit {
            let project = self.resolve_project(reference)?;
            return project["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "Missing project id".into());
        }
        {
            let state = self.state.lock().unwrap();
            if let Some(session) = p["session_id"].as_str().filter(|value| !value.is_empty()) {
                let found: Option<Option<String>> = state
                    .db
                    .query_row(
                        "SELECT project_id FROM sessions WHERE id=?1",
                        [session],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                if let Some(Some(project)) = found {
                    return Ok(project);
                }
            }
            if let Some(cwd) = p["cwd"].as_str().filter(|value| !value.is_empty()) {
                let canonical = fs::canonicalize(cwd)
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| cwd.to_string());
                if let Some(project) = project_id_for_path(&state.db, &canonical) {
                    return Ok(project);
                }
            }
        }
        let projects = self.projects()?;
        if projects.len() == 1 {
            return projects[0]["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "Missing project id".into());
        }
        let known = projects
            .iter()
            .map(|project| {
                format!(
                    "{} ({})",
                    project["name"].as_str().unwrap_or("?"),
                    project["path"].as_str().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(if known.is_empty() {
            "Navi has no projects yet. Add this folder in Navi with Add project.".into()
        } else {
            format!(
                "No Navi project contains this directory. Known projects: {known}. Name one with the `project` argument (`--project` on the CLI), or add this folder in Navi with Add project."
            )
        })
    }

    /// Find a part by id, key, or title, creating it when allowed. Models name
    /// parts in prose ("the UI"), so a slug match and a title match both count.
    fn progress_aspect(
        &self,
        project_id: &str,
        p: &Value,
        create: bool,
    ) -> Result<Option<(String, String)>> {
        let state = self.state.lock().unwrap();
        if let Some(id) = p["aspect_id"].as_str().filter(|value| !value.trim().is_empty()) {
            let title: Option<String> = state
                .db
                .query_row(
                    "SELECT title FROM aspects WHERE id=?1 AND project_id=?2",
                    params![id, project_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            return title
                .map(|title| Some((id.to_string(), title)))
                .ok_or_else(|| "Unknown part".to_string());
        }
        let name = clean_label(p["aspect"].as_str().unwrap_or_default(), 120);
        let name = name
            .strip_prefix("the ")
            .or_else(|| name.strip_prefix("The "))
            .unwrap_or(&name)
            .to_string();
        if name.is_empty() || matches!(name.to_lowercase().as_str(), "project" | "overall" | "whole project" | "all") {
            return Ok(None);
        }
        let key = slug(&name);
        let found: Option<(String, String)> = state
            .db
            .query_row(
                "SELECT id,title FROM aspects WHERE project_id=?1
                 AND (key=?2 OR lower(title)=lower(?3)) ORDER BY created LIMIT 1",
                params![project_id, key, name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(found) = found {
            return Ok(Some(found));
        }
        // "station-ui", "Station UI part", "Stations UI": the same part, so
        // a harness's spelling does not split it in two.
        let loose = |text: &str| {
            let mut word: String = text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            if let Some(stem) = word.strip_suffix("part") {
                word = stem.to_string();
            }
            word.trim_end_matches('s').to_string()
        };
        let wanted = loose(&name);
        let mut statement = state
            .db
            .prepare("SELECT id,title FROM aspects WHERE project_id=?1 ORDER BY created")
            .map_err(|error| error.to_string())?;
        let close: Option<(String, String)> = statement
            .query_map([project_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|error| error.to_string())?
            .filter_map(|row| row.ok())
            .find(|(_, title)| !wanted.is_empty() && loose(title) == wanted);
        drop(statement);
        if let Some(found) = close {
            return Ok(Some(found));
        }
        if !create {
            return Err(format!("No part named “{name}”"));
        }
        let count: i64 = state
            .db
            .query_row(
                "SELECT COUNT(*) FROM aspects WHERE project_id=?1",
                [project_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if count >= 40 {
            return Err("This project already has 40 parts; report against one of them".into());
        }
        let id = format!("asp-{}", &id()[..8]);
        state
            .db
            .execute(
                "INSERT INTO aspects(id,project_id,key,title,kind,weight,created)
                 VALUES(?1,?2,?3,?4,'work',1.0,?5)",
                params![id, project_id, key, name, now()],
            )
            .map_err(|error| error.to_string())?;
        Ok(Some((id, name)))
    }

    fn part_titles(&self, project_id: &str) -> Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        let mut statement = state
            .db
            .prepare("SELECT title FROM aspects WHERE project_id=?1 ORDER BY created")
            .map_err(|error| error.to_string())?;
        let titles = statement
            .query_map([project_id], |row| row.get(0))
            .map_err(|error| error.to_string())?
            .filter_map(|row| row.ok())
            .collect();
        Ok(titles)
    }

    /// Find a task by id or title (same part first), creating it when allowed.
    /// Returns `(id, title, state, aspect_id)`.
    fn progress_task(
        &self,
        project_id: &str,
        aspect_id: Option<&str>,
        p: &Value,
        create: bool,
    ) -> Result<(String, String, String, Option<String>)> {
        let state = self.state.lock().unwrap();
        if let Some(id) = p["task_id"].as_str().filter(|value| !value.trim().is_empty()) {
            return state
                .db
                .query_row(
                    "SELECT id,title,state,aspect_id FROM tasks WHERE id=?1 AND project_id=?2",
                    params![id, project_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|_| "Unknown task".to_string());
        }
        let title = clean_label(p["task"].as_str().unwrap_or_default(), 300);
        if title.is_empty() {
            return Err("Missing task title".into());
        }
        let found = state
            .db
            .query_row(
                "SELECT id,title,state,aspect_id FROM tasks
                 WHERE project_id=?1 AND lower(title)=lower(?2) AND state!='dropped'
                 ORDER BY (aspect_id IS ?3) DESC, created LIMIT 1",
                params![project_id, title, aspect_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(found) = found {
            return Ok(found);
        }
        if !create {
            return Err(format!("No task named “{title}”"));
        }
        let count: i64 = state
            .db
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE project_id=?1",
                [project_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if count >= 400 {
            return Err("This project already has 400 tasks".into());
        }
        let id = format!("tsk-{}", &id()[..8]);
        state
            .db
            .execute(
                "INSERT INTO tasks(id,project_id,aspect_id,title,state,weight,created,updated)
                 VALUES(?1,?2,?3,?4,'todo',1.0,?5,?5)",
                params![id, project_id, aspect_id, title, now()],
            )
            .map_err(|error| error.to_string())?;
        Ok((id, title, "todo".into(), aspect_id.map(str::to_string)))
    }

    /// `progress_report`: a harness model, the detector, or the user states how
    /// complete the whole project, one part (`aspect`), or one task is. The
    /// value is stored as an `estimate` signal with its evidence. It moves the
    /// percentage at once and stays unverified until a test, a commit, or the
    /// user agrees. Unknown parts and tasks are created unless `create=false`.
    pub fn progress_report(&self, p: &Value) -> Result<Value> {
        let project_id = self.progress_project(p)?;
        let evidence = clean_label(p["evidence"].as_str().unwrap_or_default(), 1200);
        if evidence.chars().filter(|c| c.is_alphanumeric()).count() < 3 {
            return Err("Progress needs evidence: say what you checked".into());
        }
        // An out-of-range number is a mistake to report, not a value to
        // clamp: "180" from a slip would otherwise put the project at 100%.
        let value = match (p["percent"].as_f64(), p["value"].as_f64()) {
            (Some(percent), _) if !(-0.001..=100.001).contains(&percent) => {
                return Err(format!("Percent must be between 0 and 100, not {percent}"));
            }
            (None, Some(value)) if !(-0.00001..=1.00001).contains(&value) => {
                return Err(format!("Value must be between 0 and 1, not {value}"));
            }
            (Some(percent), _) => Some(percent / 100.0),
            (None, Some(value)) => Some(value),
            _ => None,
        }
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0));
        let confidence = p["confidence"]
            .as_f64()
            .filter(|value| value.is_finite())
            .unwrap_or(0.7)
            .clamp(0.05, 1.0);
        let source = source_label(p["source"].as_str());
        let create = p["create"].as_bool().unwrap_or(true);
        let next_state = p["state"]
            .as_str()
            .map(|state| state.trim().replace([' ', '-'], "_").to_lowercase())
            .filter(|state| !state.is_empty())
            // A harness says "finished" or "started" as often as "done".
            .map(|state| match state.as_str() {
                "finished" | "complete" | "completed" | "closed" | "fixed" => "done".to_string(),
                "started" | "doing" | "wip" | "working" | "in_process" | "ongoing" => "in_progress".to_string(),
                "open" | "not_started" | "pending" | "to_do" | "reopened" => "todo".to_string(),
                "stuck" | "waiting" => "blocked".to_string(),
                _ => state,
            });
        if let Some(state) = next_state.as_deref()
            && !matches!(state, "todo" | "in_progress" | "done" | "blocked")
        {
            return Err("state must be todo, in_progress, done, or blocked".into());
        }
        let parts_before = self.part_titles(&project_id)?;
        let aspect = self.progress_aspect(&project_id, p, create)?;
        let created_part = aspect
            .as_ref()
            .filter(|(_, title)| !parts_before.iter().any(|known| known == title))
            .map(|(_, title)| title.clone());
        let mut aspect_id = aspect.as_ref().map(|(id, _)| id.clone());
        let wants_task = [&p["task_id"], &p["task"]]
            .into_iter()
            .filter_map(Value::as_str)
            .any(|value| !value.trim().is_empty());
        let mut task = None;
        let mut note = None;
        let value = if wants_task {
            let (id, title, old_state, task_aspect) =
                self.progress_task(&project_id, aspect_id.as_deref(), p, create)?;
            if aspect_id.is_none() {
                aspect_id = task_aspect;
            }
            if let Some(state) = next_state.as_deref()
                && state != old_state
            {
                let guard = self.state.lock().unwrap();
                let db = &guard.db;
                let user_owned: Option<i64> = db
                    .query_row(
                        "SELECT 1 FROM signals WHERE task_id=?1 AND kind='verdict' AND source='user' LIMIT 1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                // The user's verdict is final; a model may not overwrite it.
                if user_owned.is_some() && source != "user" {
                    note = Some("The user set this task's state, so it was left unchanged");
                } else {
                    db.execute(
                        "UPDATE tasks SET state=?1,updated=?2 WHERE id=?3",
                        params![state, now(), id],
                    )
                    .map_err(|error| error.to_string())?;
                }
            }
            task = Some((id, title));
            value.or(match next_state.as_deref() {
                Some("done") => Some(1.0),
                Some("todo") => Some(0.0),
                Some("in_progress") => Some(0.5),
                _ => None,
            })
        } else {
            value
        };
        // A user's word on a task's state is a verdict, the same as ticking its
        // box in the panel: verified, and final against later model reports.
        let kind = if source == "user" && task.is_some() && next_state.is_some() {
            "verdict"
        } else {
            "estimate"
        };
        let signal = match value {
            Some(value) => Some(self.add_signal(
                &project_id,
                task.as_ref().map(|(id, _)| id.as_str()),
                aspect_id.as_deref(),
                kind,
                &source,
                Some(value),
                confidence,
                &evidence,
            )?),
            None if task.is_some() => None,
            None => return Err("Give a percent from 0 to 100".into()),
        };
        let graph = {
            let state = self.state.lock().unwrap();
            graph_score(&state.db, &project_id)?
        };
        let aspect_title = aspect_id.as_ref().and_then(|id| {
            graph["aspects"]
                .as_array()?
                .iter()
                .find(|item| item["id"] == id.as_str())
                .and_then(|item| item["title"].as_str())
                .map(str::to_string)
        });
        let aspect_percent = aspect_id.as_ref().and_then(|id| {
            graph["aspects"]
                .as_array()?
                .iter()
                .find(|item| item["id"] == id.as_str())
                .map(|item| item["percent"].clone())
        });
        let who = source_name(&source);
        let message = match (&task, next_state.as_deref(), value) {
            (Some((_, title)), Some(state), _) => format!(
                "{who} marked “{title}” {}",
                match state {
                    "in_progress" => "in progress",
                    "todo" => "to do",
                    other => other,
                }
            ),
            (Some((_, title)), None, Some(value)) => {
                format!("{who} put “{title}” at {}%", percent(value).round())
            }
            (None, _, Some(value)) => match &aspect_title {
                Some(title) => format!("{who} put {title} at {}%", percent(value).round()),
                None => format!("{who} put the project at {}%", percent(value).round()),
            },
            _ => format!("{who} updated progress"),
        };
        let session = p["session_id"].as_str().filter(|value| !value.is_empty());
        let details = json!({
            "project_id": project_id,
            "aspect_id": aspect_id,
            "aspect": aspect_title,
            "task_id": task.as_ref().map(|(id, _)| id),
            "task": task.as_ref().map(|(_, title)| title),
            "state": next_state,
            "value": value.map(percent),
            "percent": graph["percent"],
            "aspect_percent": aspect_percent,
            "basis": graph["basis"]["kind"],
            "source": source,
            "evidence": evidence,
            "confidence": confidence,
            "signal": signal,
            "note": note,
        });
        self.event_for(Some(&project_id), session, "progress", &message, details.clone());
        let mut result = details;
        result["recorded"] = json!(true);
        result["message"] = json!(message);
        // A new part is often a misspelt old one: say so, and name the others.
        if let Some(title) = created_part {
            let others = if parts_before.is_empty() { "none".to_string() } else { parts_before.join(", ") };
            result["note"] = json!(match result["note"].as_str() {
                Some(note) => format!("{note}. This created a new part “{title}” (existing parts: {others})"),
                None => format!("This created a new part “{title}” (existing parts: {others})"),
            });
        }
        result["summary"] = json!(progress_sentence(&graph));
        Ok(result)
    }

    /// `progress_overview`: the headline of every project, for the switcher.
    pub fn progress_overview(&self) -> Result<Value> {
        let projects = self.projects()?;
        let state = self.state.lock().unwrap();
        let mut rows = Vec::new();
        for project in projects {
            let Some(id) = project["id"].as_str() else {
                continue;
            };
            let graph = graph_score(&state.db, id)?;
            rows.push(json!({
                "project_id": id,
                "percent": graph["percent"],
                "basis": graph["basis"]["kind"],
                "confidence": graph["confidence"],
                "verified": graph["verified"],
                "commits_since": graph["commits_since"],
            }));
        }
        Ok(json!({"projects": rows}))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_signal(
        &self,
        project_id: &str,
        task_id: Option<&str>,
        aspect_id: Option<&str>,
        kind: &str,
        source: &str,
        value: Option<f64>,
        confidence: f64,
        evidence: &str,
    ) -> Result<i64> {
        let state = self.state.lock().unwrap();
        insert_signal(
            &state.db,
            project_id,
            task_id,
            aspect_id,
            kind,
            source,
            value,
            confidence,
            evidence,
        )
    }

    /// The newest estimates and verdicts one source recorded for a project,
    /// with the part and task titles they name.
    pub fn source_reports(&self, project_id: &str, source: &str, limit: usize) -> Result<Vec<Value>> {
        let state = self.state.lock().unwrap();
        let mut query = state
            .db
            .prepare(
                "SELECT s.kind, a.title, t.title, s.value, s.evidence, s.time FROM signals s
                 LEFT JOIN aspects a ON a.id=s.aspect_id LEFT JOIN tasks t ON t.id=s.task_id
                 WHERE s.project_id=?1 AND s.source=?2 AND s.kind IN ('estimate','verdict')
                 ORDER BY s.id DESC LIMIT ?3",
            )
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map(params![project_id, source, limit as i64], |row| {
                Ok(json!({
                    "kind": row.get::<_, String>(0)?,
                    "part": row.get::<_, Option<String>>(1)?,
                    "task": row.get::<_, Option<String>>(2)?,
                    "value": row.get::<_, Option<f64>>(3)?,
                    "evidence": row.get::<_, String>(4)?,
                    "time": row.get::<_, u64>(5)?,
                }))
            })
            .map_err(|error| error.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        Ok(rows)
    }

    /// The newest signal id, so a change can name the rows it added.
    pub fn last_signal_id(&self) -> Result<i64> {
        let state = self.state.lock().unwrap();
        state
            .db
            .query_row("SELECT COALESCE(MAX(id),0) FROM signals", [], |row| row.get(0))
            .map_err(|error| error.to_string())
    }

    /// Take back one change the user made through Navi: the user signals it
    /// added (ids in `after..=until`) and, for a task, its previous state.
    pub fn undo_change(&self, change: &Value) -> Result<usize> {
        let project_id = change["project_id"].as_str().ok_or("Missing project")?;
        let state = self.state.lock().unwrap();
        let removed = state
            .db
            .execute(
                "DELETE FROM signals WHERE project_id=?1 AND id>?2 AND id<=?3 AND source='user'",
                params![project_id, change["after"].as_i64().unwrap_or(i64::MAX), change["until"].as_i64().unwrap_or(0)],
            )
            .map_err(|error| error.to_string())?;
        if let (Some(task), Some(previous)) = (change["task_id"].as_str(), change["task_state"].as_str()) {
            state
                .db
                .execute(
                    "UPDATE tasks SET state=?1,updated=?2 WHERE id=?3 AND project_id=?4",
                    params![previous, now(), task, project_id],
                )
                .map_err(|error| error.to_string())?;
        }
        // A part added by mistake goes again, unless tasks were put in it.
        if let Some(aspect) = change["aspect_added"].as_str() {
            state
                .db
                .execute(
                    "DELETE FROM aspects WHERE id=?1 AND project_id=?2
                     AND NOT EXISTS (SELECT 1 FROM tasks WHERE aspect_id=?1 AND state!='dropped')",
                    params![aspect, project_id],
                )
                .map_err(|error| error.to_string())?;
        }
        drop(state);
        let mut details = change.clone();
        details["source"] = json!("user");
        self.event_for(Some(project_id), None, "progress", "Undid the last progress change", details);
        Ok(removed)
    }

    /// Store one captured pane screen unless the newest stored screen for the
    /// session is byte-identical (BUG-087). The hash, not the timestamp, is the
    /// key: a sampler that reads the same screen ten times writes one row, so a
    /// single test result can never become ten signals.
    pub fn record_tail(&self, session_id: &str, project_id: Option<&str>, text: &str) -> Result<Value> {
        let hash = tail_hash(text);
        let lines = text.lines().count() as i64;
        let state = self.state.lock().unwrap();
        let previous: Option<(i64, String)> = state
            .db
            .query_row(
                "SELECT id,hash FROM session_tails WHERE session_id=?1 ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(id) = previous
            .filter(|(_, previous)| previous == &hash)
            .map(|(id, _)| id)
        {
            return Ok(json!({
                "id": id,
                "hash": hash,
                "lines": lines,
                "changed": false,
                "duplicate": true,
            }));
        }
        let time = now();
        state
            .db
            .execute(
                "INSERT INTO session_tails(session_id,project_id,hash,lines,text,time)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![session_id, project_id, hash, lines, text, time],
            )
            .map_err(|error| error.to_string())?;
        let id = state.db.last_insert_rowid();
        let _ = state.db.execute(
            "DELETE FROM session_tails WHERE session_id=?1 AND id NOT IN
                (SELECT id FROM session_tails WHERE session_id=?1 ORDER BY id DESC LIMIT ?2)",
            params![session_id, TAIL_ROWS_PER_SESSION],
        );
        Ok(json!({
            "id": id,
            "hash": hash,
            "lines": lines,
            "changed": true,
            "time": time,
        }))
    }

    pub fn history(&self, p: &Value) -> Result<Value> {
        let limit = p["limit"].as_u64().unwrap_or(200).min(500) as i64;
        let before = p["before"]
            .as_u64()
            .map(|value| value.min(i64::MAX as u64) as i64)
            .unwrap_or(i64::MAX);
        let project = p["project_id"].as_str();
        let session = p["session_id"].as_str();
        let state = self.state.lock().unwrap();
        let mut sql = String::from(
            "SELECT seq,epoch,project_id,session_id,time,kind,message,details FROM events WHERE seq < ?1",
        );
        if project.is_some() {
            sql.push_str(" AND project_id=?2");
        }
        if session.is_some() {
            sql.push_str(if project.is_some() {
                " AND session_id=?3"
            } else {
                " AND session_id=?2"
            });
        }
        sql.push_str(" ORDER BY seq DESC LIMIT ?");
        sql.push_str(if project.is_some() && session.is_some() {
            "4"
        } else if project.is_some() || session.is_some() {
            "3"
        } else {
            "2"
        });
        let mut statement = state.db.prepare(&sql).map_err(|error| error.to_string())?;
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(before)];
        if let Some(project) = project {
            values.push(Box::new(project.to_string()));
        }
        if let Some(session) = session {
            values.push(Box::new(session.to_string()));
        }
        values.push(Box::new(limit));
        let references = values
            .iter()
            .map(|value| value.as_ref() as &dyn rusqlite::ToSql)
            .collect::<Vec<_>>();
        let rows = statement
            .query_map(references.as_slice(), |row| {
                Ok(json!({
                    "seq": row.get::<_, i64>(0)?,
                    "epoch": row.get::<_, String>(1)?,
                    "project_id": row.get::<_, Option<String>>(2)?,
                    "session_id": row.get::<_, Option<String>>(3)?,
                    "time": row.get::<_, u64>(4)?,
                    "kind": row.get::<_, String>(5)?,
                    "message": row.get::<_, String>(6)?,
                    "details": row.get::<_, String>(7)?,
                }))
            })
            .map_err(|error| error.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let mut rows = rows;
        rows.reverse();
        Ok(json!({"events":rows,"has_more":rows.len() as i64 >= limit}))
    }

    pub fn save_message(&self, p: &Value) -> Result<Value> {
        let id = p["id"].as_str().ok_or("Missing message id")?;
        let role = p["role"].as_str().ok_or("Missing message role")?;
        if !matches!(role, "user" | "assistant" | "system" | "tool") {
            return Err("Invalid message role".into());
        }
        let parts = p.get("parts").cloned().unwrap_or(json!([]));
        let state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "INSERT OR REPLACE INTO messages(id,project_id,session_id,turn,role,parts,time)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    id,
                    p["project_id"].as_str(),
                    p["session_id"].as_str(),
                    p["turn"].as_str(),
                    role,
                    parts.to_string(),
                    p["time"].as_u64().unwrap_or_else(now)
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({"id":id}))
    }

    /// Newest page of chat history, ascending, with a `rowid` cursor per
    /// message. `before` pages backwards: the smallest cursor of a page is
    /// the `before` of the next one, so nothing repeats or goes missing even
    /// when several messages share a timestamp (BUG-047).
    pub fn messages(&self, p: &Value) -> Result<Value> {
        let limit = p["limit"].as_u64().unwrap_or(200).min(500) as i64;
        let before = p["before"]
            .as_u64()
            .map(|value| value.min(i64::MAX as u64) as i64)
            .unwrap_or(i64::MAX);
        let project = p["project_id"].as_str();
        let state = self.state.lock().unwrap();
        let mut sql = String::from(
            "SELECT rowid,id,project_id,session_id,turn,role,parts,time FROM messages \
             WHERE rowid < ?1",
        );
        if project.is_some() {
            sql.push_str(" AND project_id=?2");
        }
        // Newest first so the limit keeps the page nearest the cursor, plus
        // one extra row that only answers `has_more`.
        sql.push_str(" ORDER BY rowid DESC LIMIT ?");
        sql.push_str(if project.is_some() { "3" } else { "2" });
        let mut statement = state.db.prepare(&sql).map_err(|error| error.to_string())?;
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(before)];
        if let Some(project) = project {
            values.push(Box::new(project.to_string()));
        }
        values.push(Box::new(limit + 1));
        let references = values
            .iter()
            .map(|value| value.as_ref() as &dyn rusqlite::ToSql)
            .collect::<Vec<_>>();
        let mut rows = statement
            .query_map(references.as_slice(), |row| {
                Ok(json!({
                    "cursor": row.get::<_, i64>(0)?,
                    "id": row.get::<_, String>(1)?,
                    "project_id": row.get::<_, Option<String>>(2)?,
                    "session_id": row.get::<_, Option<String>>(3)?,
                    "turn": row.get::<_, Option<String>>(4)?,
                    "role": row.get::<_, String>(5)?,
                    "parts": serde_json::from_str::<Value>(&row.get::<_, String>(6)?).unwrap_or(json!([])),
                    "time": row.get::<_, u64>(7)?,
                }))
            })
            .map_err(|error| error.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        rows.reverse();
        Ok(json!({"messages":rows,"has_more":has_more}))
    }

    pub fn session_events(&self, p: &Value) -> Result<Value> {
        let session = p["id"].as_str().ok_or("Missing session")?;
        let limit = p["limit"].as_u64().unwrap_or(200).min(500) as i64;
        let state = self.state.lock().unwrap();
        let mut statement = state
            .db
            .prepare(
                "SELECT id,time,kind,payload FROM session_events WHERE session=?1
                 ORDER BY id LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let events = statement
            .query_map(params![session, limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "time": row.get::<_, u64>(1)?,
                    "kind": row.get::<_, String>(2)?,
                    "payload": serde_json::from_str::<Value>(&row.get::<_, String>(3)?)
                        .unwrap_or(json!({})),
                }))
            })
            .map_err(|error| error.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        Ok(json!({"events":events}))
    }

    /// Flat window geometry: writes take {width,height,maximized,monitor}
    /// fields directly (unknown keys rejected), reads return the same object
    /// with nulls for anything unset. Kept out of settings.
    pub fn window_state(&self, p: &Value) -> Result<Value> {
        let state = self.state.lock().unwrap();
        if let Some(map) = p.as_object()
            && !map.is_empty()
        {
            let known = ["width", "height", "maximized", "monitor"];
            if let Some(key) = map.keys().find(|key| !known.contains(&key.as_str())) {
                return Err(format!("Unknown window_state key {key}"));
            }
            let mut value = json!({"width":null,"height":null,"maximized":null,"monitor":null});
            for key in known {
                if let Some(field) = p.get(key) {
                    value[key] = field.clone();
                }
            }
            for key in ["width", "height"] {
                if !value[key].is_null()
                    && value[key]
                        .as_u64()
                        .is_none_or(|number| !(320..=16384).contains(&number))
                {
                    return Err(format!("Invalid window {key}"));
                }
            }
            if !value["maximized"].is_null() && !value["maximized"].is_boolean() {
                return Err("Invalid window maximized".into());
            }
            if !value["monitor"].is_null()
                && value["monitor"].as_u64().is_none_or(|m| m > 255)
            {
                return Err("Invalid window monitor".into());
            }
            state
                .db
                .execute(
                    "INSERT OR REPLACE INTO window_state(id,value) VALUES(1,?1)",
                    [value.to_string()],
                )
                .map_err(|error| error.to_string())?;
            return Ok(value);
        }
        let stored: Option<String> = state
            .db
            .query_row("SELECT value FROM window_state WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| error.to_string())?;
        Ok(stored
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(json!({"width":null,"height":null,"maximized":null,"monitor":null})))
    }

    /// Remember which OpenCode session the assistant used for a project so the
    /// next turn (and the UI) can report it. Best effort after a turn lands.
    pub fn record_assistant_session(
        &self,
        project_id: &str,
        session_id: &str,
        model: &str,
        agent: &str,
    ) -> Result<()> {
        let state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "INSERT INTO assistant_sessions(project_id,session_id,model,agent,updated)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(project_id) DO UPDATE SET
                    session_id=excluded.session_id, model=excluded.model,
                    agent=excluded.agent, updated=excluded.updated",
                params![project_id, session_id, model, agent, now()],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Newest signal, task update, or checkpoint time for a project; None when
    /// nothing has been recorded yet.
    pub fn project_last_activity(&self, project_id: &str) -> Result<Option<u64>> {
        let state = self.state.lock().unwrap();
        let times: Vec<Option<u64>> = [
            "SELECT MAX(time) FROM signals WHERE project_id=?1",
            "SELECT MAX(updated) FROM tasks WHERE project_id=?1",
            "SELECT MAX(created) FROM checkpoints WHERE project_id=?1",
        ]
        .iter()
        .map(|sql| -> Result<Option<u64>> {
            state
                .db
                .query_row(sql, [project_id], |row| row.get::<_, Option<u64>>(0))
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
        Ok(times.into_iter().flatten().max())
    }

}

fn git_state(path: &str) -> (Option<String>, Option<String>, bool) {
    let root = git(path, &["rev-parse", "--show-toplevel"]).map(|text| text.trim().to_string());
    let Some(root) = root.filter(|root| !root.is_empty()) else {
        return (None, None, false);
    };
    let head = git(&root, &["rev-parse", "HEAD"]).map(|text| text.trim().to_string());
    let dirty = git(&root, &["status", "--porcelain=v1", "-z"])
        .map(|text| !text.is_empty())
        .unwrap_or(false);
    (Some(root), head, dirty)
}

pub(crate) fn git(path: &str, arguments: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let output = child.wait_with_output().ok()?;
                return Some(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            Ok(Some(_)) => {
                let _ = child.wait();
                return None;
            }
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// "1 task", "3 tasks".
fn counted(count: u64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// "1 task has", "3 tasks have".
fn counted_have(count: u64, noun: &str) -> String {
    format!("{} {}", counted(count, noun), if count == 1 { "has" } else { "have" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let database = Connection::open_in_memory().unwrap();
        database.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        database.execute_batch(SCHEMA).unwrap();
        database
            .execute(
                "INSERT INTO projects(id,name,path,created) VALUES('p','P','/tmp/p',1)",
                [],
            )
            .unwrap();
        database
    }

    fn aspect(db: &Connection, id: &str, weight: f64) {
        db.execute(
            "INSERT INTO aspects(id,project_id,key,title,weight,created)
             VALUES(?1,'p',?1,?1,?2,1)",
            params![id, weight],
        )
        .unwrap();
    }

    fn task(db: &Connection, id: &str, aspect_id: &str, weight: f64) {
        db.execute(
            "INSERT INTO tasks(id,project_id,aspect_id,title,weight,created,updated)
             VALUES(?1,'p',?2,?1,?3,1,1)",
            params![id, aspect_id, weight],
        )
        .unwrap();
    }

    #[test]
    fn legacy_database_migrates_and_backfills_projects() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE settings (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, profile TEXT NOT NULL,
                     cwd TEXT NOT NULL, created INTEGER NOT NULL
                 );
                 INSERT INTO sessions VALUES('s1','n','p','/tmp',1);",
            )
            .unwrap();
        let settings = json!({"projects":[{"name":"P","path":"/tmp"}]});
        migrate(&database, &settings).unwrap();
        let count: i64 = database
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let project: Option<String> = database
            .query_row("SELECT project_id FROM sessions WHERE id='s1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(project.is_some());
        let version: i64 = database
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn project_mean_uses_aspect_weights() {
        let database = setup();
        aspect(&database, "a1", 3.0);
        aspect(&database, "a2", 1.0);
        task(&database, "t1", "a1", 1.0);
        task(&database, "t2", "a2", 1.0);
        insert_signal(
            &database,
            "p",
            Some("t1"),
            Some("a1"),
            "test_pass",
            "harness",
            Some(1.0),
            1.0,
            "PASSED 12",
        )
        .unwrap();
        let score = graph_score_at(&database, "p", now()).unwrap();
        assert_eq!(score["percent"], 75.0);
    }

    #[test]
    fn empty_project_reports_null_not_zero() {
        let database = setup();
        let score = graph_score_at(&database, "p", 1_000).unwrap();
        assert_eq!(score["percent"], Value::Null);
        assert_eq!(score["percent_null"], true);
        assert_eq!(score["confidence"], 0.0);
    }

    #[test]
    fn weighted_completion_and_verified_share() {
        let database = setup();
        aspect(&database, "a1", 1.0);
        aspect(&database, "a2", 3.0);
        task(&database, "t1", "a1", 1.0);
        task(&database, "t2", "a2", 3.0);
        insert_signal(&database, "p", Some("t1"), Some("a1"), "test_pass", "harness", Some(1.0), 1.0, "PASSED 12").unwrap();
        let score = graph_score_at(&database, "p", now()).unwrap();
        assert_eq!(score["percent"], 25.0);
        assert_eq!(score["verified"], 25.0);
        assert_eq!(score["confidence"], 0.5);
        assert_eq!(score["tasks"]["total"], 2);
    }

    #[test]
    fn empty_aspect_leaves_the_denominator() {
        let database = setup();
        aspect(&database, "a1", 1.0);
        aspect(&database, "empty", 9.0);
        task(&database, "t1", "a1", 1.0);
        insert_signal(&database, "p", Some("t1"), Some("a1"), "verdict", "user", Some(1.0), 1.0, "done").unwrap();
        let score = graph_score_at(&database, "p", now()).unwrap();
        assert_eq!(score["percent"], 100.0);
    }

    #[test]
    fn assistant_claims_cap_and_decay() {
        let database = setup();
        aspect(&database, "a1", 1.0);
        task(&database, "t1", "a1", 1.0);
        let day = 86_400_000_u64;
        let at = now();
        insert_signal(&database, "p", Some("t1"), Some("a1"), "assistant_claim", "assistant", Some(1.0), 1.0, "claim").unwrap();
        let fresh = graph_score_at(&database, "p", at).unwrap();
        assert_eq!(fresh["percent"], 50.0);
        let old = graph_score_at(&database, "p", at + 8 * day).unwrap();
        assert_eq!(old["percent"], 25.0);
    }

    #[test]
    fn user_verdict_beats_newer_non_user_signals() {
        let database = setup();
        aspect(&database, "a1", 1.0);
        task(&database, "t1", "a1", 1.0);
        insert_signal(&database, "p", Some("t1"), Some("a1"), "verdict", "user", Some(1.0), 1.0, "done").unwrap();
        insert_signal(&database, "p", Some("t1"), Some("a1"), "assistant_claim", "assistant", Some(0.0), 1.0, "not done").unwrap();
        let score = graph_score_at(&database, "p", now()).unwrap();
        assert_eq!(score["percent"], 100.0);
    }
}
