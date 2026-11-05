//! Project detector: a bounded scan of a project's files plus a model
//! assessment of how complete it is.
//!
//! The scan finds what a person would look at first: the languages and size of
//! the code, its parts (workspace members or top-level code directories), the
//! written checklists in its markdown, TODO and stub markers, tests, CI, and
//! git history. It never reads more than a fixed budget of files.
//!
//! The assessment is a model's judgment, not arithmetic. When a linked harness
//! runs in the project, Navi types an assessment request into it and the
//! harness reports through the progress link as it inspects the code. Without
//! one, Navi's own assistant is asked and its `PROGRESS` lines are recorded.
//! When that assistant cannot answer either (OpenCode neither reachable nor
//! installed), Navi opens the first installed linked harness in the project
//! with the assessment as its first prompt.

use crate::{App, Result, link, now, store};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 8000;
const MAX_DEPTH: usize = 8;
const MAX_TEXT_FILES: usize = 700;
const MAX_MARKDOWN_FILES: usize = 80;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_PARTS: usize = 8;

const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", "dist", "build", "out", "vendor", "__pycache__",
    "venv", "env", "coverage", "Pods", "DerivedData", "site-packages", "bower_components",
];

const WORKSPACE_DIRS: &[&str] = &[
    "packages", "apps", "crates", "services", "libs", "modules", "plugins", "extensions",
];

const MANIFESTS: &[&str] = &[
    "package.json", "Cargo.toml", "pyproject.toml", "setup.py", "go.mod", "pom.xml",
    "build.gradle", "build.gradle.kts", "Gemfile", "composer.json", "Package.swift",
    "pubspec.yaml", "mix.exs", "CMakeLists.txt", "Makefile", "deno.json",
];

fn language(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "rs" => "Rust",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" => "Python",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "scala" => "Scala",
        "dart" => "Dart",
        "lua" => "Lua",
        "zig" => "Zig",
        "ex" | "exs" => "Elixir",
        "hs" => "Haskell",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "html" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "gd" => "GDScript",
        "glsl" | "wgsl" | "frag" | "vert" => "Shader",
        _ => return None,
    })
}

fn is_test_path(relative: &str) -> bool {
    let lower = relative.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.split('/').any(|part| matches!(part, "test" | "tests" | "__tests__" | "spec" | "e2e"))
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_test.")
        || name.starts_with("test_")
}

#[derive(Default)]
struct Checklist {
    file: String,
    heading: String,
    done: usize,
    total: usize,
    open: Vec<String>,
}

#[derive(Default)]
struct Scan {
    entries: usize,
    truncated: bool,
    code_files: usize,
    code_lines: usize,
    languages: BTreeMap<&'static str, usize>,
    test_files: usize,
    markdown: Vec<PathBuf>,
    markers: usize,
    stubs: usize,
    marker_samples: Vec<String>,
    ci: bool,
    readme: Option<(String, u64)>,
    manifests: Vec<String>,
    /// Code files per first-level and second-level directory.
    dir_files: HashMap<String, usize>,
    text_files_read: usize,
}

fn hidden(name: &str) -> bool {
    name.starts_with('.') && name != ".github"
}

fn walk(root: &Path, dir: &Path, depth: usize, scan: &mut Scan) {
    if depth > MAX_DEPTH || scan.entries >= MAX_ENTRIES {
        scan.truncated |= scan.entries >= MAX_ENTRIES;
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        scan.entries += 1;
        if scan.entries >= MAX_ENTRIES {
            scan.truncated = true;
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| name.clone());
        if kind.is_dir() {
            if name == ".github" && path.join("workflows").is_dir() {
                scan.ci = true;
            }
            if hidden(&name) || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, depth + 1, scan);
            continue;
        }
        if depth == 0 {
            if MANIFESTS.contains(&name.as_str()) {
                scan.manifests.push(name.clone());
            }
            if matches!(name.as_str(), ".gitlab-ci.yml" | ".travis.yml" | "azure-pipelines.yml" | "Jenkinsfile") {
                scan.ci = true;
            }
            if name.to_lowercase().starts_with("readme") {
                let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                scan.readme = Some((name.clone(), size));
            }
        }
        let extension = name.rsplit_once('.').map(|(_, ext)| ext.to_lowercase()).unwrap_or_default();
        if matches!(extension.as_str(), "md" | "markdown") {
            if scan.markdown.len() < MAX_MARKDOWN_FILES {
                scan.markdown.push(path.clone());
            }
            continue;
        }
        let Some(language) = language(&extension) else { continue };
        if name.ends_with(".min.js") || name.contains(".bundle.") || name.ends_with(".generated.ts") {
            continue;
        }
        scan.code_files += 1;
        *scan.languages.entry(language).or_default() += 1;
        if is_test_path(&relative) {
            scan.test_files += 1;
        }
        let mut parts = relative.split('/');
        if let (Some(first), Some(second), Some(_)) = (parts.next(), parts.next(), parts.next()) {
            *scan.dir_files.entry(first.to_string()).or_default() += 1;
            *scan.dir_files.entry(format!("{first}/{second}")).or_default() += 1;
        } else if let Some(first) = relative.split('/').next().filter(|_| relative.contains('/')) {
            *scan.dir_files.entry(first.to_string()).or_default() += 1;
        }
        if scan.text_files_read < MAX_TEXT_FILES {
            read_code(&path, &relative, scan);
        }
    }
}

fn read_text(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_code(path: &Path, relative: &str, scan: &mut Scan) {
    let Some(text) = read_text(path) else { return };
    scan.text_files_read += 1;
    scan.code_lines += text.lines().count();
    for (number, line) in text.lines().enumerate() {
        let marker = ["TODO", "FIXME", "XXX", "HACK"].iter().any(|word| {
            line.match_indices(word).any(|(index, _)| {
                let before = line[..index].chars().next_back();
                let after = line[index + word.len()..].chars().next();
                before.is_none_or(|c| !c.is_alphanumeric())
                    && after.is_none_or(|c| !c.is_alphanumeric())
            })
        });
        let stub = line.contains("unimplemented!(")
            || line.contains("todo!(")
            || line.contains("NotImplementedError")
            || line.to_lowercase().contains("not implemented");
        if marker {
            scan.markers += 1;
        }
        if stub {
            scan.stubs += 1;
        }
        if (marker || stub) && scan.marker_samples.len() < 12 {
            let snippet: String = line.trim().chars().take(100).collect();
            scan.marker_samples.push(format!("{relative}:{}: {snippet}", number + 1));
        }
    }
}

/// Markdown task lists outside code fences, grouped by their nearest heading.
fn checklists(root: &Path, files: &[PathBuf]) -> Vec<Checklist> {
    let mut lists: Vec<Checklist> = Vec::new();
    for path in files {
        let Some(text) = read_text(path) else { continue };
        let file = path
            .strip_prefix(root)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut fence: Option<&str> = None;
        let mut heading = String::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            if let Some(open) = fence {
                if trimmed.starts_with(open) {
                    fence = None;
                }
                continue;
            }
            if trimmed.starts_with("```") {
                fence = Some("```");
                continue;
            }
            if trimmed.starts_with("~~~") {
                fence = Some("~~~");
                continue;
            }
            if let Some(title) = trimmed.strip_prefix('#') {
                heading = title.trim_start_matches('#').trim().chars().take(80).collect();
                continue;
            }
            let item = ["- [", "* [", "+ ["]
                .iter()
                .find_map(|prefix| trimmed.strip_prefix(prefix));
            let Some(rest) = item else { continue };
            let (done, title) = match rest.get(..2) {
                Some("x]") | Some("X]") => (true, &rest[2..]),
                Some(" ]") => (false, &rest[2..]),
                _ => continue,
            };
            let index = match lists
                .iter()
                .position(|list| list.file == file && list.heading == heading)
            {
                Some(index) => index,
                None => {
                    lists.push(Checklist {
                        file: file.clone(),
                        heading: heading.clone(),
                        ..Checklist::default()
                    });
                    lists.len() - 1
                }
            };
            let list = &mut lists[index];
            list.total += 1;
            if done {
                list.done += 1;
            } else if list.open.len() < 6 {
                list.open.push(title.trim().chars().take(120).collect());
            }
        }
    }
    lists
}

/// Candidate parts: workspace members when the repository has them,
/// otherwise the top-level directories that hold most of the code.
fn parts(root: &Path, scan: &Scan) -> Vec<(String, String, usize)> {
    let mut found: Vec<(String, String, usize)> = Vec::new();
    for workspace in WORKSPACE_DIRS {
        let Ok(entries) = fs::read_dir(root.join(workspace)) else { continue };
        for entry in entries.filter_map(|entry| entry.ok()) {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if hidden(&name) {
                continue;
            }
            let relative = format!("{workspace}/{name}");
            let files = scan.dir_files.get(&relative).copied().unwrap_or(0);
            if files > 0 {
                found.push((name, relative, files));
            }
        }
    }
    if found.is_empty() {
        let mut top: Vec<(String, usize)> = scan
            .dir_files
            .iter()
            .filter(|(dir, _)| !dir.contains('/'))
            .filter(|(dir, _)| !matches!(dir.as_str(), "test" | "tests" | "docs" | "doc" | "examples" | "scripts" | "benches" | "fixtures"))
            .map(|(dir, files)| (dir.clone(), *files))
            .collect();
        top.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        // One dominant source directory: its subdirectories are the parts.
        if let Some((dir, files)) = top.first()
            && (top.len() == 1 || *files * 2 > scan.code_files)
            && matches!(dir.as_str(), "src" | "lib" | "app" | "source" | "Sources")
        {
            let prefix = format!("{dir}/");
            for (sub, count) in &scan.dir_files {
                if let Some(name) = sub.strip_prefix(&prefix)
                    && !name.contains('/')
                    && *count >= 2
                {
                    found.push((name.to_string(), sub.clone(), *count));
                }
            }
        }
        if found.is_empty() {
            for (dir, files) in top {
                found.push((dir.clone(), dir, files));
            }
        }
    }
    found.sort_by(|left, right| right.2.cmp(&left.2).then(left.0.cmp(&right.0)));
    found.truncate(MAX_PARTS);
    found
}

fn title_case(name: &str) -> String {
    let spaced = name.replace(['-', '_'], " ");
    let mut out = String::new();
    for word in spaced.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// `project_scan`: the bounded scan as JSON, plus a one-line digest a model
/// can use as a starting point.
pub fn scan_project(path: &str) -> Result<Value> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        return Err("The project directory is missing".into());
    }
    let started = now();
    let mut scan = Scan::default();
    walk(&root, &root, 0, &mut scan);
    let markdown = std::mem::take(&mut scan.markdown);
    let lists = checklists(&root, &markdown);
    let parts = parts(&root, &scan);
    let done: usize = lists.iter().map(|list| list.done).sum();
    let total: usize = lists.iter().map(|list| list.total).sum();
    let mut languages: Vec<(&str, usize)> = scan.languages.iter().map(|(k, v)| (*k, *v)).collect();
    languages.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let commits = store::git(path, &["rev-list", "--count", "HEAD"])
        .and_then(|text| text.trim().parse::<u64>().ok());
    let last_commit = store::git(path, &["log", "-1", "--format=%ct"])
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(|seconds| seconds * 1000);
    let dirty = store::git(path, &["status", "--porcelain", "--untracked-files=no"])
        .map(|text| text.lines().count());
    let mut digest = Vec::new();
    if !languages.is_empty() {
        digest.push(format!(
            "{} code files ({})",
            scan.code_files,
            languages
                .iter()
                .take(4)
                .map(|(name, count)| format!("{name} {count}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        digest.push("no source code found".into());
    }
    digest.push(format!("{} test files", scan.test_files));
    if total > 0 {
        digest.push(format!("checklists {done}/{total} done"));
    }
    if scan.markers + scan.stubs > 0 {
        digest.push(format!("{} TODO/FIXME markers, {} stubs", scan.markers, scan.stubs));
    }
    digest.push(if scan.ci { "CI configured".into() } else { "no CI".to_string() });
    if let Some(count) = commits {
        digest.push(format!("{count} commits"));
    }
    Ok(json!({
        "path": path,
        "elapsed_ms": now().saturating_sub(started),
        "truncated": scan.truncated,
        "entries": scan.entries,
        "code_files": scan.code_files,
        "lines_read": scan.code_lines,
        "files_read": scan.text_files_read,
        "languages": languages.iter().map(|(name, count)| json!({"name":name,"files":count})).collect::<Vec<_>>(),
        "test_files": scan.test_files,
        "markers": scan.markers,
        "stubs": scan.stubs,
        "marker_samples": scan.marker_samples,
        "ci": scan.ci,
        "readme": scan.readme.as_ref().map(|(name, size)| json!({"name":name,"bytes":size})),
        "manifests": scan.manifests,
        "git": {"commits": commits, "last_commit": last_commit, "changed_files": dirty},
        "checklists": lists.iter().map(|list| json!({
            "file": list.file, "heading": list.heading, "done": list.done,
            "total": list.total, "open": list.open,
        })).collect::<Vec<_>>(),
        "checklist_done": done,
        "checklist_total": total,
        "parts": parts.iter().map(|(name, path, files)| {
            // Two members with one name ("apps/navi", "packages/navi") keep
            // their workspace so the parts stay distinguishable.
            let shared = parts.iter().filter(|(other, _, _)| other == name).count() > 1;
            let workspace = path.split('/').next().unwrap_or_default();
            let title = if shared && path.contains('/') {
                format!("{} ({workspace})", title_case(name))
            } else {
                title_case(name)
            };
            json!({"name": title, "path": path, "files": files})
        }).collect::<Vec<_>>(),
        "digest": digest.join("; "),
    }))
}

/// Case-insensitive ASCII prefix strip that never splits a character.
fn strip_word<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    words.iter().find_map(|word| {
        let head = text.get(..word.len())?;
        head.eq_ignore_ascii_case(word).then(|| &text[word.len()..])
    })
}

/// `TASK "UI" todo "Add a settings page" | why`: a remaining task the
/// assistant named during an assessment. Returns `(part, state, title,
/// evidence)`; the state is one of todo, in_progress, done, blocked.
pub fn parse_task_lines(text: &str) -> Vec<(String, &'static str, String, String)> {
    fn quoted(text: &str) -> Option<(String, &str)> {
        let text = text.trim_start();
        let inner = text.strip_prefix('"').or_else(|| text.strip_prefix('“'))?;
        let end = inner.find(['"', '”'])?;
        let close = inner[end..].chars().next().map_or(1, char::len_utf8);
        Some((inner[..end].trim().to_string(), &inner[end + close..]))
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*', '>', '`', ' '])
            .trim_end_matches('`');
        let Some(rest) = line.strip_prefix("TASK").or_else(|| line.strip_prefix("Task:")) else {
            continue;
        };
        let rest = rest.trim_start_matches(':');
        let (head, evidence) = match rest.split_once('|') {
            Some((head, evidence)) => (head, evidence.trim().to_string()),
            None => (rest, String::new()),
        };
        let Some((part, after)) = quoted(head) else { continue };
        let after = after.trim_start();
        let (state, after) = [("in progress", "in_progress"), ("in_progress", "in_progress"),
            ("todo", "todo"), ("to do", "todo"), ("done", "done"), ("blocked", "blocked")]
            .into_iter()
            .find_map(|(word, state)| strip_word(after, &[word]).map(|tail| (state, tail)))
            .unwrap_or(("todo", after));
        let Some((title, _)) = quoted(after) else { continue };
        if part.is_empty() || title.is_empty() {
            continue;
        }
        out.push((part, state, title, evidence));
    }
    out
}

/// `PROGRESS overall 55 | evidence` / `PROGRESS part "UI" 60 | evidence`.
/// Returns `(part, percent, evidence)`; part `None` is the whole project.
pub fn parse_progress_lines(text: &str) -> Vec<(Option<String>, f64, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*', '>', '`', ' '])
            .trim_end_matches('`');
        let Some(rest) = line
            .strip_prefix("PROGRESS")
            .or_else(|| line.strip_prefix("Progress:"))
            .or_else(|| line.strip_prefix("progress"))
        else {
            continue;
        };
        let rest = rest.trim_start_matches(':').trim();
        let (head, evidence) = match rest.split_once('|') {
            Some((head, evidence)) => (head.trim(), evidence.trim().to_string()),
            None => (rest, String::new()),
        };
        let (part, tail) = if let Some(tail) =
            strip_word(head, &["overall", "project", "whole project", "total"])
        {
            (None, tail.trim())
        } else if let Some(tail) = strip_word(head, &["part", "aspect", "area"]) {
            let tail = tail.trim();
            if let Some(quoted) = tail.strip_prefix('"').or_else(|| tail.strip_prefix('“')) {
                let end = quoted.find(['"', '”']).unwrap_or(quoted.len());
                let name = quoted[..end].trim().to_string();
                let after = quoted.get(end + quoted[end..].chars().next().map_or(0, char::len_utf8)..).unwrap_or("");
                (Some(name), after.trim())
            } else {
                // Unquoted: everything before the last number is the name.
                let split = tail
                    .rfind(|c: char| c.is_whitespace())
                    .unwrap_or(0);
                (Some(tail[..split].trim().trim_end_matches(':').to_string()), tail[split..].trim())
            }
        } else {
            continue;
        };
        let number: String = tail
            .trim_start_matches([':', '=', ' '])
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let Ok(percent) = number.parse::<f64>() else { continue };
        if !(0.0..=100.0).contains(&percent) || part.as_ref().is_some_and(|name| name.is_empty()) {
            continue;
        }
        out.push((part, percent, evidence));
    }
    out
}

impl App {
    pub fn project_scan(&self, p: &Value) -> Result<Value> {
        let project_id = self.progress_project(p)?;
        let project = self.project_by_id(&project_id)?;
        let path = project["path"].as_str().unwrap_or_default().to_string();
        let mut scan = scan_project(&path)?;
        scan["project_id"] = json!(project_id);
        Ok(scan)
    }

    /// `project_detect`: scan, record what the files alone can support, then
    /// ask a model for the real judgment.
    pub fn project_detect(&self, p: &Value) -> Result<Value> {
        let project_id = self.progress_project(p)?;
        let project = self.project_by_id(&project_id)?;
        let name = project["name"].as_str().unwrap_or("project").to_string();
        let path = project["path"].as_str().unwrap_or_default().to_string();
        let _ = self.probe_project(&project_id);
        let scan = scan_project(&path)?;
        self.event_for(
            Some(&project_id),
            None,
            "progress",
            &format!("Scanned {name}: {}", scan["digest"].as_str().unwrap_or("")),
            json!({"project_id":project_id,"scan":true,"digest":scan["digest"],"source":"detector"}),
        );
        let graph = self.graph_get(Some(&project_id))?;
        // Checklists are a weak signal (often stale), so they only seed a
        // project nobody has estimated yet and never outrank a model.
        let modeled = graph["estimate"]["source"]
            .as_str()
            .is_some_and(|source| source != "detector");
        let mut seeded = Value::Null;
        let done = scan["checklist_done"].as_u64().unwrap_or(0);
        let total = scan["checklist_total"].as_u64().unwrap_or(0);
        if !modeled && total >= 5 {
            let files: Vec<&str> = scan["checklists"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|list| list["file"].as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .take(3)
                .collect();
            let evidence = format!(
                "{done} of {total} checklist items checked in {}",
                files.join(", ")
            );
            if graph["estimate"]["evidence"].as_str() != Some(evidence.as_str()) {
                seeded = self.progress_report(&json!({
                    "project_id": project_id,
                    "percent": done as f64 * 100.0 / total as f64,
                    "evidence": evidence,
                    "confidence": 0.35,
                    "source": "detector",
                }))?;
            }
        }
        let known_parts: Vec<String> = graph["aspects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|aspect| aspect["title"].as_str().map(str::to_string))
            .collect();
        let parts = if known_parts.is_empty() {
            scan["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["name"].as_str().map(str::to_string))
                .collect()
        } else {
            known_parts
        };
        let digest = scan["digest"].as_str().unwrap_or_default().to_string();
        // A linked harness already running in this project does the judging:
        // it reads the code, and each report lands as it is made.
        let sessions = self.sessions()?;
        let mut candidates: Vec<&Value> = sessions
            .iter()
            .filter(|session| session["project_id"] == project_id.as_str())
            .filter(|session| session["running"] == true)
            .filter(|session| {
                matches!(
                    crate::adapters::adapter_for(session["harness"].as_str().unwrap_or("")),
                    "claude" | "codex" | "opencode" | "pi"
                )
            })
            .collect();
        candidates.sort_by_key(|session| {
            (
                session["state"] == "busy",
                session["active"] != true,
                std::cmp::Reverse(session["last_active"].as_u64().unwrap_or(0)),
            )
        });
        let requested = p["via"].as_str().unwrap_or("auto");
        if requested != "assistant"
            && let Some(session) = candidates.first()
        {
            let target = session["id"].as_str().unwrap_or_default().to_string();
            let harness = session["harness"].as_str().unwrap_or("harness").to_string();
            let adapter = crate::adapters::adapter_for(&harness);
            if session["state"] == "busy" {
                return Ok(json!({
                    "project_id": project_id, "scan": scan, "seeded": seeded,
                    "via": "harness", "started": false, "session": target,
                    "message": format!("{} is busy in {name}. Ask again when it finishes, or ask Navi's assistant instead.", store::source_name(adapter)),
                }));
            }
            let prompt = link::assessment_prompt(adapter, &parts, &digest);
            self.inject(&target, &prompt, true)?;
            let message = format!(
                "Asked {} to assess {name}. Its estimates will appear as it reports.",
                store::source_name(adapter)
            );
            self.event_for(
                Some(&project_id),
                Some(&target),
                "progress",
                &message,
                json!({"project_id":project_id,"assessment":"harness","session":target,"source":adapter}),
            );
            return Ok(json!({
                "project_id": project_id, "scan": scan, "seeded": seeded,
                "via": "harness", "started": true, "session": target,
                "harness": harness, "message": message,
            }));
        }
        // No harness runs here and Navi's assistant cannot answer: open an
        // installed harness in the project and let it judge through the link,
        // instead of failing on a missing OpenCode.
        if requested != "assistant" && !self.assistant_available() {
            let Some(profile) = self.assessment_profile() else {
                return Err(format!(
                    "Nothing can judge {name} yet: Navi's assistant (OpenCode) is not available and no linked harness is installed. Install OpenCode, Claude Code, Codex or Pi."
                ));
            };
            let executable = profile["executable"].as_str().unwrap_or_default();
            let harness = profile["harness"].as_str().unwrap_or_else(|| {
                Path::new(executable).file_name().and_then(|name| name.to_str()).unwrap_or("")
            });
            let adapter = crate::adapters::adapter_for(harness);
            let label = profile["name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| store::source_name(adapter));
            let prompt = link::assessment_prompt(adapter, &parts, &digest);
            let session = self.create_session(&json!({
                "profile": profile["id"], "cwd": path, "prompt": prompt,
            }))?;
            let target = session["id"].as_str().unwrap_or_default().to_string();
            let message = format!(
                "Opened {label} to assess {name}, since Navi's assistant is not available. Its estimates will appear as it reports."
            );
            self.event_for(
                Some(&project_id),
                Some(&target),
                "progress",
                &message,
                json!({"project_id":project_id,"assessment":"launch","session":target,"source":adapter}),
            );
            return Ok(json!({
                "project_id": project_id, "scan": scan, "seeded": seeded,
                "via": "launch", "started": true, "session": target,
                "harness": harness, "message": message,
            }));
        }
        // No harness: Navi's own assistant judges in the background, and its
        // PROGRESS lines are recorded when the turn ends.
        let app = self.me.upgrade().ok_or("Navi is shutting down")?;
        let params = json!({"project_id": project_id});
        let parts_line = if parts.is_empty() {
            "Pick 3 to 7 parts that describe the project's areas.".to_string()
        } else {
            format!("Use these parts: {}.", parts.join(", "))
        };
        let prompt = format!(
            "Progress check for {name}. Inspect the repository (read the README, docs, and the code; run nothing that changes files) and judge how complete it is against its own goals. {parts_line} Scan facts: {digest}. Reply with one short spoken sentence, then exactly one line per part and one overall line in this form:\nPROGRESS part \"<part>\" <percent> | <what you checked>\nPROGRESS overall <percent> | <what you checked>\nThen, for each part below 100%, up to three concrete remaining tasks:\nTASK \"<part>\" todo \"<task>\" | <why it is needed>"
        );
        std::thread::spawn(move || {
            let outcome = app.assistant_assessment(&params, &prompt);
            if let Err(error) = outcome {
                app.event_for(
                    params["project_id"].as_str(),
                    None,
                    "warning",
                    &format!("Progress assessment failed: {error}"),
                    json!({"project_id":params["project_id"]}),
                );
            }
        });
        let message = format!("Assessing {name}. Navi's assistant is reading the project.");
        self.event_for(
            Some(&project_id),
            None,
            "progress",
            &message,
            json!({"project_id":project_id,"assessment":"assistant","source":"navi"}),
        );
        Ok(json!({
            "project_id": project_id, "scan": scan, "seeded": seeded,
            "via": "assistant", "started": true, "message": message,
        }))
    }

    /// Record the `PROGRESS` lines of an assistant reply. Parts first, then
    /// the overall line, so the model's own headline is the newest estimate.
    pub fn record_progress_lines(&self, project_id: &str, text: &str) -> Result<usize> {
        let mut lines = parse_progress_lines(text);
        lines.sort_by_key(|(part, _, _)| part.is_none());
        let mut recorded = 0;
        for (part, percent, evidence) in lines {
            let evidence = if evidence.is_empty() {
                "assistant assessment".to_string()
            } else {
                evidence
            };
            let result = self.progress_report(&json!({
                "project_id": project_id,
                "aspect": part,
                "percent": percent,
                "evidence": evidence,
                "confidence": 0.6,
                "source": "navi",
            }));
            if result.is_ok() {
                recorded += 1;
            }
        }
        // Remaining tasks follow the estimates; they never move a part an
        // estimate leads, but they make "what's left" concrete.
        for (part, state, title, evidence) in parse_task_lines(text) {
            let _ = self.progress_report(&json!({
                "project_id": project_id,
                "aspect": part,
                "task": title,
                "state": state,
                "evidence": if evidence.is_empty() { "assistant assessment".to_string() } else { evidence },
                "confidence": 0.6,
                "source": "navi",
            }));
        }
        Ok(recorded)
    }
}
