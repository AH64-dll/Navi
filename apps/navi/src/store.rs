//! Durable project/work graph and event history.
//!
//! A project is a row, not a settings entry. Completion is always derived from
//! the append-only `signals` ledger by `graph_score`; nothing stores a percent.

use crate::{App, Result, id, now};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
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
    aspect_id: Option<String>,
    state: String,
    weight: f64,
    score: f64,
    freshness: f64,
    verified: bool,
    covered: bool,
    unknown: bool,
}

struct SignalRow {
    _id: i64,
    kind: String,
    source: String,
    value: Option<f64>,
    confidence: f64,
    time: u64,
}

fn score_task(db: &Connection, task_id: &str, at: u64) -> Result<(TaskScore, String)> {
    let mut statement = db
        .prepare(
            "SELECT id,kind,source,value,confidence,time FROM signals
             WHERE task_id=?1 ORDER BY time DESC, id DESC",
        )
        .map_err(|error| error.to_string())?;
    let signals = statement
        .query_map([task_id], |row| {
            Ok(SignalRow {
                _id: row.get(0)?,
                kind: row.get(1)?,
                source: row.get(2)?,
                value: row.get(3)?,
                confidence: row.get(4)?,
                time: row.get(5)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let verdict = signals
        .iter()
        .find(|signal| signal.kind == "verdict" && signal.source == "user");
    let chosen = verdict.or_else(|| signals.first());
    let Some(chosen) = chosen else {
        return Ok((
            TaskScore {
                aspect_id: None,
                state: String::new(),
                weight: 1.0,
                score: 0.0,
                freshness: 0.0,
                verified: false,
                covered: false,
                unknown: true,
            },
            String::new(),
        ));
    };
    let age_days = at.saturating_sub(chosen.time) as f64 / 86_400_000.0;
    let freshness = 0.5_f64.powf(age_days / 10.0);
    let verified = chosen.source != "assistant"
        && !matches!(chosen.kind.as_str(), "assistant_claim" | "touched");
    let score = match chosen.kind.as_str() {
        "verdict" => chosen.value.unwrap_or(0.0).clamp(0.0, 1.0),
        "test_pass" | "test_fail" | "commit" | "checkpoint" | "git_commit" => {
            chosen.value.unwrap_or(1.0).clamp(0.0, 1.0)
        }
        "assistant_claim" => {
            let decay = if age_days > 7.0 { 0.5 } else { 1.0 };
            0.5 * chosen.confidence.clamp(0.0, 1.0) * decay
        }
        "touched" => 0.5 * chosen.confidence.clamp(0.0, 1.0),
        _ => chosen.value.unwrap_or(0.0).clamp(0.0, 1.0),
    };
    Ok((
        TaskScore {
            aspect_id: None,
            state: String::new(),
            weight: 1.0,
            score: score.clamp(0.0, 1.0),
            freshness,
            verified,
            covered: true,
            unknown: false,
        },
        chosen.kind.clone(),
    ))
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
        .prepare("SELECT id,aspect_id,title,state,weight FROM tasks WHERE project_id=?1")
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

    let mut grouped: HashMap<String, Vec<TaskScore>> = HashMap::new();
    let mut counts = json!({"total":0,"todo":0,"in_progress":0,"done":0,"blocked":0,"dropped":0});
    let mut unassigned = 0;
    let mut total_weight = 0.0;
    let mut verified_weight = 0.0;
    let mut covered_tasks = 0;
    let mut total_tasks = 0;
    let mut freshness_sum = 0.0;
    let mut unknown_tasks = 0;
    for (id, aspect_id, _title, state, weight) in tasks {
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
        let Some(aspect) = aspect_id else {
            unassigned += 1;
            continue;
        };
        let (mut score, selected_kind) = score_task(db, &id, at)?;
        score.state = state;
        score.weight = weight.max(0.0);
        score.aspect_id = Some(aspect.clone());
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
        let _ = selected_kind;
        grouped.entry(aspect).or_default().push(score);
    }

    let mut aspect_values = Vec::new();
    let mut project_score = 0.0;
    let mut project_weight = 0.0;
    let mut contributing = 0;
    let has_aspects = !aspects.is_empty();
    for (id, key, title, kind, weight) in aspects {
        let scores = grouped.remove(&id).unwrap_or_default();
        let tasks_weight: f64 = scores.iter().map(|task| task.weight).sum();
        let aspect_score = if scores.is_empty() {
            0.0
        } else {
            scores.iter().map(|task| task.score * task.weight).sum::<f64>() / tasks_weight
        };
        let aspect_verified = if scores.is_empty() {
            0.0
        } else {
            scores
                .iter()
                .filter(|task| task.verified)
                .map(|task| task.weight)
                .sum::<f64>()
                / tasks_weight
        };
        let aspect_freshness = if scores.is_empty() {
            0.0
        } else {
            scores.iter().map(|task| task.freshness).sum::<f64>() / scores.len() as f64
        };
        if !scores.is_empty() {
            contributing += 1;
            project_score += aspect_score * weight.max(0.0);
            project_weight += weight.max(0.0);
        }
        aspect_values.push(json!({
            "id": id,
            "key": key,
            "title": title,
            "kind": kind,
            "weight": weight,
            "percent": if scores.is_empty() { Value::Null } else { json!(percent(aspect_score)) },
            "percent_null": scores.is_empty(),
            "verified": percent(aspect_verified),
            "confidence": (aspect_freshness * (scores.iter().filter(|task| task.covered).count() as f64 / scores.len().max(1) as f64)),
            "tasks": scores.len(),
            "unknown": scores.iter().filter(|task| task.unknown).count(),
        }));
    }

    let percent_value = if project_weight > 0.0 {
        percent(project_score / project_weight)
    } else {
        0.0
    };
    let verified = if total_weight > 0.0 {
        percent(verified_weight / total_weight)
    } else {
        0.0
    };
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
    let confidence = (coverage * freshness).clamp(0.0, 1.0);

    let mut risks = Vec::new();
    if !has_aspects {
        risks.push("No aspects defined".to_string());
    }
    if unknown_tasks > 0 {
        risks.push(format!("{unknown_tasks} task(s) have no evidence"));
    }
    if unassigned > 0 {
        risks.push(format!("{unassigned} task(s) have no aspect"));
    }
    if counts["blocked"].as_u64().unwrap_or(0) > 0 {
        risks.push(format!(
            "{} task(s) are blocked",
            counts["blocked"].as_u64().unwrap_or(0)
        ));
    }
    // Every number is derived from these rows, so the payload carries them:
    // a percentage whose evidence cannot be shown is a bug (BUG-084/089).
    let mut signal_statement = db
        .prepare(
            "SELECT id,task_id,aspect_id,kind,source,value,confidence,evidence,time
             FROM signals WHERE project_id=?1 ORDER BY time DESC, id DESC LIMIT 40",
        )
        .map_err(|error| error.to_string())?;
    let signals = signal_statement
        .query_map([project_id], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "task_id": row.get::<_, Option<String>>(1)?,
                "aspect_id": row.get::<_, Option<String>>(2)?,
                "kind": row.get::<_, String>(3)?,
                "source": row.get::<_, String>(4)?,
                "value": row.get::<_, Option<f64>>(5)?,
                "confidence": row.get::<_, f64>(6)?,
                "evidence": row.get::<_, String>(7)?,
                "time": row.get::<_, u64>(8)?,
                "assistant": row.get::<_, String>(4)? == "assistant",
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
        "percent": if project_weight > 0.0 { json!(percent_value) } else { Value::Null },
        "percent_null": project_weight <= 0.0,
        "verified": verified,
        "confidence": (confidence * 1000.0).round() / 1000.0,
        "as_of": at,
        "why": format!("{covered_tasks} of {total_tasks} tasks carry evidence; {contributing} aspect(s) contribute"),
        "aspects": aspect_values,
        "tasks": counts,
        "risks": risks,
        "signals": signals,
        "signal_count": signal_count,
    }))
}

pub fn graph_score(db: &Connection, project_id: &str) -> Result<Value> {
    graph_score_at(db, project_id, now())
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
            let evidence = match head.as_deref() {
                Some(head) => format!("HEAD {head}"),
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
                    Some(if next_state == "done" { 1.0 } else { 0.0 }),
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

    pub fn progress_claim(&self, p: &Value) -> Result<Value> {
        let project = self.resolve_project(p["project_id"].as_str().ok_or("Missing project")?)?;
        let project_id = project["id"].as_str().ok_or("Missing project id")?;
        let evidence = p["evidence"].as_str().unwrap_or_default().trim();
        if evidence.is_empty() {
            return Err("A progress claim needs evidence".into());
        }
        let value = p["value"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
        let confidence = p["confidence"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
        let id = self.add_signal(
            project_id,
            p["task_id"].as_str(),
            p["aspect_id"].as_str(),
            "assistant_claim",
            "assistant",
            Some(value),
            confidence,
            evidence,
        )?;
        Ok(json!({"signal":id,"capped":0.5*confidence}))
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
