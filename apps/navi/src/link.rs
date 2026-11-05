//! The harness link: how a coding harness running in a Navi pane reads and
//! moves the project's completion.
//!
//! Every pane Navi launches carries `NAVI_PROJECT_ID`, `NAVI_SESSION_ID`,
//! `NAVI_HARNESS`, and the station's directories in its environment. Harnesses
//! that speak MCP get this binary registered as a stdio server (`navid mcp`);
//! Pi is told to call `navid progress` from its shell tool. Both paths end in
//! the daemon's `progress_report`, so a model's judgment of "the UI is about
//! 60% done" lands in the graph the moment the model states it.

use crate::{Result, rpc};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Newest MCP revision this server speaks; older ones it also accepts.
const PROTOCOL: &str = "2025-06-18";
const SUPPORTED: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Tool names as Claude Code exposes them, for `--allowedTools`.
const CLAUDE_TOOLS: &str = "mcp__navi__navi_progress,mcp__navi__navi_report_progress,mcp__navi__navi_track_task";

/// Server instructions: what the model is told when the link starts.
pub const INSTRUCTIONS: &str = "Navi is the user's desktop station. It tracks how complete this project is, overall and per part, and shows it on screen and by voice. Keep it accurate: after finishing meaningful work, or when asked how far along the project is, call navi_progress to see what is recorded, then navi_report_progress with your honest estimate for each part you assessed and for the whole project. Use navi_track_task when you start, finish, or get blocked on a concrete task. Evidence must name what you checked (files read, tests run, behavior verified). Never inflate: untested or stubbed work is not done.";

/// Instructions for harnesses without MCP, which call the CLI instead.
fn cli_instructions(cli: &str) -> String {
    format!(
        "Navi is the user's desktop station. It tracks how complete this project is, overall and per part. Keep it accurate with your shell tool: after meaningful work, or when asked how far along the project is, run `{cli} progress` to see what is recorded, then report your honest estimate: `{cli} progress report <percent> \"<what you checked>\" --part \"<part>\"` (omit --part for the whole project). For a concrete task: `{cli} progress task <todo|in_progress|done|blocked> \"<task title>\" \"<evidence>\" --part \"<part>\"`. Evidence must name what you checked. Never inflate: untested or stubbed work is not done."
    )
}

/// The project, session, and harness this process runs for, from the pane
/// environment Navi set at launch.
fn context() -> Value {
    let read = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    json!({
        "project_id": read("NAVI_PROJECT_ID"),
        "session_id": read("NAVI_SESSION_ID"),
        "source": read("NAVI_HARNESS").unwrap_or_else(|| "assistant".into()),
        "cwd": std::env::current_dir().ok().map(|path| path.to_string_lossy().into_owned()),
    })
}

fn station(op: &str, params: Value) -> Result<Value> {
    let reply = rpc(json!({"op": op, "params": params})).map_err(|error| {
        if error.contains("No such file") || error.contains("Connection refused") {
            "Navi is not running, so progress cannot be read or recorded".to_string()
        } else {
            error
        }
    })?;
    if reply["ok"] == true {
        Ok(reply["result"].clone())
    } else {
        Err(reply["error"]
            .as_str()
            .unwrap_or("Navi rejected the request")
            .to_string())
    }
}

/// Models send numbers as `60`, `"60"`, or `"60%"`; all mean sixty percent.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().trim_end_matches('%').trim().parse().ok(),
        _ => None,
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "navi_progress",
            "title": "Read project progress",
            "description": "Read Navi's completion record for this project: the overall percent and its basis, each part with its percent, and the task checklist. Call it before reporting so your numbers build on what is recorded.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "navi_report_progress",
            "title": "Report project progress",
            "description": "Record how complete the whole project, or one part of it, is in your judgment. Omit `part` for the whole project. Name parts by area (\"UI\", \"Voice\", \"Storage\"); a new name creates the part. Evidence must say what you checked. Untested or stubbed work is not done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "percent": {"type": "number", "minimum": 0, "maximum": 100, "description": "Completion from 0 to 100."},
                    "part": {"type": "string", "description": "Part of the project, such as \"UI\". Omit for the whole project."},
                    "evidence": {"type": "string", "description": "What you checked that supports the number: files read, tests run, behavior verified."},
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1, "description": "How sure you are, from 0 to 1. Default 0.7."}
                },
                "required": ["percent", "evidence"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
        },
        {
            "name": "navi_track_task",
            "title": "Track a task",
            "description": "Add a task to Navi's checklist or change its state when you start, finish, or get blocked on a concrete piece of work. Tasks match by title; a new title creates the task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Short task title, such as \"Add login form\"."},
                    "state": {"type": "string", "enum": ["todo", "in_progress", "done", "blocked"]},
                    "part": {"type": "string", "description": "Part the task belongs to, such as \"UI\"."},
                    "evidence": {"type": "string", "description": "What happened: the change made, the test that passed, or the blocker."},
                    "percent": {"type": "number", "minimum": 0, "maximum": 100, "description": "Optional completion of this task."}
                },
                "required": ["task", "state", "evidence"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
        }
    ])
}

fn report_text(result: &Value) -> String {
    let mut text = result["message"].as_str().unwrap_or("Recorded").to_string();
    if let Some(note) = result["note"].as_str() {
        text.push_str(&format!(". {note}"));
    }
    if let Some(summary) = result["summary"].as_str() {
        text.push_str(&format!(". Now: {summary}"));
    }
    text
}

fn call_tool(params: &Value) -> Value {
    let arguments = &params["arguments"];
    let outcome = match params["name"].as_str().unwrap_or_default() {
        "navi_progress" => station("progress_get", context())
            .map(|result| result["text"].as_str().unwrap_or_default().to_string()),
        "navi_report_progress" => {
            let mut request = context();
            request["percent"] = json!(number(&arguments["percent"]));
            request["aspect"] = arguments["part"].clone();
            request["evidence"] = arguments["evidence"].clone();
            if let Some(confidence) = number(&arguments["confidence"]) {
                request["confidence"] = json!(confidence);
            }
            if request["percent"].is_null() {
                Err("percent must be a number from 0 to 100".to_string())
            } else {
                station("progress_report", request).map(|result| report_text(&result))
            }
        }
        "navi_track_task" => {
            let mut request = context();
            request["task"] = arguments["task"].clone();
            request["state"] = arguments["state"].clone();
            request["aspect"] = arguments["part"].clone();
            request["evidence"] = arguments["evidence"].clone();
            if let Some(percent) = number(&arguments["percent"]) {
                request["percent"] = json!(percent);
            }
            station("progress_report", request).map(|result| report_text(&result))
        }
        other => Err(format!("Unknown tool {other}")),
    };
    match outcome {
        Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
        Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
    }
}

/// One JSON-RPC message in, at most one reply out. Notifications and
/// responses to requests this server never sends get no reply.
fn handle(message: Value) -> Option<Value> {
    let id = message.get("id").cloned().filter(|id| !id.is_null())?;
    let method = message["method"].as_str()?;
    let result = match method {
        "initialize" => {
            let requested = message["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL);
            let version = if SUPPORTED.contains(&requested) { requested } else { PROTOCOL };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "navi", "title": "Navi progress", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call_tool(&message["params"])),
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => Err(json!({"code": -32601, "message": format!("Method not found: {method}")})),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    })
}

/// `navid mcp`: newline-delimited JSON-RPC 2.0 over stdio.
pub fn mcp() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let replies: Vec<Value> = match serde_json::from_str::<Value>(line) {
            Ok(Value::Array(batch)) => batch.into_iter().filter_map(handle).collect(),
            Ok(message) => handle(message).into_iter().collect(),
            Err(_) => vec![json!({"jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": "Parse error"}})],
        };
        for reply in replies {
            writeln!(out, "{reply}").map_err(|error| error.to_string())?;
            out.flush().map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

const PROGRESS_USAGE: &str = "navid progress                                  show this project's progress
navid progress report <percent> \"<evidence>\" [--part NAME] [--confidence 0-1]
navid progress task <todo|in_progress|done|blocked> \"<title>\" \"<evidence>\" [--part NAME]";

/// `navid progress …`: the same link for harnesses without MCP.
pub fn progress(args: &[String]) -> Result<()> {
    let mut positional = Vec::new();
    let mut options: BTreeMap<&str, String> = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--part" | "--confidence" | "--project" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{arg} needs a value\n{PROGRESS_USAGE}"))?;
                options.insert(arg, value.clone());
                index += 2;
            }
            "-h" | "--help" => {
                println!("{PROGRESS_USAGE}");
                return Ok(());
            }
            _ => {
                positional.push(arg.to_string());
                index += 1;
            }
        }
    }
    let mut request = context();
    if let Some(project) = options.get("--project") {
        request["project_id"] = json!(project);
    }
    if let Some(part) = options.get("--part") {
        request["aspect"] = json!(part);
    }
    let output = match positional.first().map(String::as_str) {
        None | Some("show") => station("progress_get", request)?["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        Some("report" | "set") => {
            let percent = positional
                .get(1)
                .and_then(|value| number(&json!(value)))
                .ok_or_else(|| format!("report needs a percent\n{PROGRESS_USAGE}"))?;
            request["percent"] = json!(percent);
            request["evidence"] = json!(positional[2..].join(" "));
            if let Some(confidence) = options.get("--confidence") {
                request["confidence"] = json!(number(&json!(confidence)));
            }
            report_text(&station("progress_report", request)?)
        }
        Some("task") => {
            let state = positional
                .get(1)
                .ok_or_else(|| format!("task needs a state\n{PROGRESS_USAGE}"))?;
            let title = positional
                .get(2)
                .ok_or_else(|| format!("task needs a title\n{PROGRESS_USAGE}"))?;
            request["state"] = json!(state);
            request["task"] = json!(title);
            request["evidence"] = json!(positional[3..].join(" "));
            report_text(&station("progress_report", request)?)
        }
        Some(other) => return Err(format!("Unknown progress command {other}\n{PROGRESS_USAGE}")),
    };
    println!("{output}");
    Ok(())
}

/// TOML basic string for a codex `-c key=value` override.
fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// What a launch adds so the harness can reach the link: extra argv placed
/// right after the executable, and extra environment.
pub struct Registration {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Build the per-harness registration. `link_env` is the `NAVI_*` set every
/// pane receives; MCP servers get it explicitly because some harnesses start
/// their servers with a scrubbed environment.
pub fn registration(
    adapter: &str,
    exe: &Path,
    link_env: &BTreeMap<String, String>,
    existing_env: &BTreeMap<String, String>,
) -> Registration {
    let exe_text = exe.to_string_lossy().into_owned();
    let mut argv = Vec::new();
    let mut env = BTreeMap::new();
    match adapter {
        "claude" => {
            let config = json!({"mcpServers": {"navi": {
                "type": "stdio", "command": exe_text, "args": ["mcp"], "env": link_env,
            }}});
            // `--mcp-config` and `--allowedTools` are variadic, so each is
            // followed by another option that ends its value list.
            argv.extend([
                "--mcp-config".to_string(),
                config.to_string(),
                "--allowedTools".to_string(),
                CLAUDE_TOOLS.to_string(),
                "--append-system-prompt".to_string(),
                INSTRUCTIONS.to_string(),
            ]);
        }
        "codex" => {
            let table = link_env
                .iter()
                .map(|(key, value)| format!("{key}={}", toml_string(value)))
                .collect::<Vec<_>>()
                .join(",");
            argv.extend([
                "-c".to_string(),
                format!("mcp_servers.navi.command={}", toml_string(&exe_text)),
                "-c".to_string(),
                "mcp_servers.navi.args=[\"mcp\"]".to_string(),
                "-c".to_string(),
                format!("mcp_servers.navi.env={{{table}}}"),
            ]);
        }
        "opencode" => {
            let mut config: Value = existing_env
                .get("OPENCODE_CONFIG_CONTENT")
                .and_then(|text| serde_json::from_str(text).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            if !config["mcp"].is_object() {
                config["mcp"] = json!({});
            }
            config["mcp"]["navi"] = json!({
                "type": "local", "command": [exe_text, "mcp"], "environment": link_env, "enabled": true,
            });
            env.insert("OPENCODE_CONFIG_CONTENT".into(), config.to_string());
        }
        "pi" => {
            let cli = if exe.file_name().and_then(|name| name.to_str()) == Some("navid") {
                "navid".to_string()
            } else {
                exe_text.clone()
            };
            argv.extend(["--append-system-prompt".to_string(), cli_instructions(&cli)]);
        }
        _ => {}
    }
    Registration { argv, env }
}

/// The prompt Navi types into a running harness to ask for an assessment.
pub fn assessment_prompt(adapter: &str, parts: &[String], facts: &str) -> String {
    let parts_line = if parts.is_empty() {
        "Choose 3 to 7 parts that describe this project's areas.".to_string()
    } else {
        format!(
            "Assess these parts (rename or add parts only if they are clearly wrong): {}.",
            parts.join(", ")
        )
    };
    let how = match adapter {
        "claude" | "codex" | "opencode" => {
            "Record each part with the navi_report_progress tool (percent, part, evidence), then the whole project without a part.".to_string()
        }
        _ => "Record each part with `navid progress report <percent> \"<evidence>\" --part \"<part>\"`, then the whole project without --part.".to_string(),
    };
    let mut prompt = format!(
        "Navi progress check. Inspect this repository and judge how complete it is against its own goals (README, docs, issues, TODOs, tests). {parts_line} {how} Base every number on what you actually read or ran, keep it short, and do not change any files."
    );
    if !facts.is_empty() {
        prompt.push_str(&format!(" Scan facts: {facts}"));
    }
    prompt
}

const LINK_USAGE: &str = "navid link [claude|codex|opencode|pi]   print how to link a harness started outside Navi";

/// This binary's path, for harness configs and pane environments. Linux
/// reports a binary replaced while it runs (an upgrade, a rebuild) as
/// "/path/navid (deleted)"; the replacement at the original path is the one
/// to name, so the marker is dropped when that path still exists.
pub fn navid_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    if let Some(original) = exe.to_str().and_then(|text| text.strip_suffix(" (deleted)")) {
        let original = PathBuf::from(original);
        if original.is_file() {
            return Ok(original);
        }
    }
    Ok(exe)
}

/// Harnesses `navid link` knows how to set up.
pub const LINK_HARNESSES: [&str; 4] = ["claude", "codex", "opencode", "pi"];

/// The one-time setup that links a harness started outside Navi. Panes Navi
/// opens are linked per launch; a harness started by hand needs Navi's MCP
/// server in its own config, and the daemon then resolves the project from
/// the harness's working directory. Returns (title, where the snippet goes,
/// snippet). Nothing is written, so no config changes without the user.
pub fn setup(harness: &str) -> Result<(String, String, String)> {
    if !LINK_HARNESSES.contains(&harness) {
        return Err(format!("Unknown harness {harness}\n{LINK_USAGE}"));
    }
    let exe = navid_path()?;
    let exe_text = exe.to_string_lossy().into_owned();
    // Only directory overrides in effect now; a default install needs none.
    let mut env: BTreeMap<String, String> = ["NAVI_ISOLATED", "NAVI_DATA_DIR", "NAVI_RUNTIME_DIR"]
        .into_iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| (key.to_string(), value))
        })
        .collect();
    env.insert("NAVI_HARNESS".to_string(), harness.to_string());
    Ok(match harness {
        "claude" => {
            let flags = env
                .iter()
                .map(|(key, value)| format!("-e {key}={}", shell_word(value)))
                .collect::<Vec<_>>()
                .join(" ");
            (
                "Claude Code".into(),
                "Run once in a terminal, then allow the three tools under permissions.allow in ~/.claude/settings.json.".into(),
                format!(
                    "claude mcp add --scope user navi {flags} -- {} mcp\n\n\"{}\"",
                    shell_word(&exe_text),
                    CLAUDE_TOOLS.replace(',', "\", \"")
                ),
            )
        }
        "codex" => {
            let table = env
                .iter()
                .map(|(key, value)| format!("{key} = {}", toml_string(value)))
                .collect::<Vec<_>>()
                .join(", ");
            (
                "Codex".into(),
                "Add to ~/.codex/config.toml.".into(),
                format!(
                    "[mcp_servers.navi]\ncommand = {}\nargs = [\"mcp\"]\nenv = {{ {table} }}",
                    toml_string(&exe_text)
                ),
            )
        }
        "opencode" => {
            let config = json!({"mcp": {"navi": {
                "type": "local", "command": [exe_text, "mcp"], "environment": env, "enabled": true,
            }}});
            (
                "OpenCode".into(),
                "Merge into ~/.config/opencode/opencode.json.".into(),
                serde_json::to_string_pretty(&config).unwrap_or_default(),
            )
        }
        _ => {
            let mut cli: Vec<String> = env
                .iter()
                .map(|(key, value)| format!("{key}={}", shell_word(value)))
                .collect();
            cli.push(shell_word(&exe_text));
            (
                "Pi".into(),
                "Pi has no MCP. Add this to the project's AGENTS.md, or ~/.pi/agent/AGENTS.md for every project.".into(),
                cli_instructions(&cli.join(" ")),
            )
        }
    })
}

/// `navid link [harness]`: print `setup` for one harness or all of them.
pub fn link(args: &[String]) -> Result<()> {
    let wanted = args.first().map(String::as_str);
    if matches!(wanted, Some("-h" | "--help")) {
        println!("{LINK_USAGE}");
        return Ok(());
    }
    let mut sections = Vec::new();
    for harness in wanted.map_or(LINK_HARNESSES.to_vec(), |name| vec![name]) {
        let (title, place, snippet) = setup(harness)?;
        sections.push(format!("{title}: {place}\n{}", indent(&snippet)));
    }
    println!(
        "{}\n\nStart the harness inside a project Navi knows (or a folder below it); reports land on that project.",
        sections.join("\n\n")
    );
    Ok(())
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
}

/// Quote for a POSIX shell only when needed.
fn shell_word(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/._-=:,+@".contains(character))
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}
