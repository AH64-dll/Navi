use crate::{App, Result, now, store};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Coalescing window for the live answer: at most one `reply_delta` per window,
/// carrying the text of the whole turn so far. 500 deltas in 300 ms become a
/// handful of replace-events instead of overflowing the client (BUG-098).
const DELTA_FLUSH_MS: u64 = 120;
const DELTA_FLUSH_CHARS: usize = 240;
/// When the "decision model unavailable" warning was last shown.
static CHIEF_FALLBACK_WARNED: AtomicU64 = AtomicU64::new(0);

pub struct PersonaSwitch {
    pub name: String,
    pub reply: String,
}

pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    let rest = endpoint
        .strip_prefix("http://127.0.0.1:")
        .ok_or("The chief must use a local 127.0.0.1 HTTP endpoint")?;
    let (port, path) = rest.split_once('/').ok_or("Missing decision API path")?;
    if port.parse::<u16>().ok().filter(|p| *p > 0).is_none() || path != "v1/systemone" {
        return Err("Expected http://127.0.0.1:<port>/v1/systemone".into());
    }
    Ok(())
}

#[cfg(test)]
pub fn answer(
    response: &Value,
    options: &serde_json::Map<String, Value>,
    minimum: f64,
) -> Result<String> {
    let answers = response
        .get("answers")
        .or_else(|| response.get("results"))
        .unwrap_or(response);
    let action = &answers["action"];
    let selected = action["value"]
        .as_str()
        .or_else(|| action["choice"].as_str())
        .or_else(|| action["answer"].as_str())
        .or_else(|| action.as_str())
        .ok_or(format!("Unsupported decision response: {response}"))?;
    if !options.contains_key(selected) {
        return Err("Model selected an unregistered action".into());
    }
    let probability = action["probabilities"][selected].as_f64().or_else(|| {
        action["probability"]
            .as_f64()
            .or_else(|| action["confidence"].as_f64())
    });
    if !matches!(selected, "noop" | "clarify")
        && probability.is_none_or(|score| score < minimum || score > 1.0 || !score.is_finite())
    {
        return Ok("clarify".into());
    }
    Ok(selected.into())
}

impl App {
    fn desktop_request(&self, action: &Value, generation: u64) -> Result<Value> {
        if self.isolated && action["type"] != "windows" {
            return Err("Live desktop input is disabled in isolated mode".into());
        }
        let mut stream = UnixStream::connect(self.runtime.join("desktop.sock"))
            .map_err(|error| format!("Desktop disconnected: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let mut request = action.clone();
        request["expires"] = json!(now() + 2000);
        request["generation"] = json!(generation);
        writeln!(stream, "{request}").map_err(|error| error.to_string())?;
        let mut reply = String::new();
        BufReader::new(stream)
            .take(crate::MAX_MESSAGE as u64)
            .read_line(&mut reply)
            .map_err(|error| error.to_string())?;
        serde_json::from_str(&reply).map_err(|error| error.to_string())
    }

    fn ensure_chief(&self, generation: u64) -> Result<()> {
        let settings = self.settings();
        let config = &settings["model"];
        let endpoint = config["endpoint"]
            .as_str()
            .ok_or("Missing model endpoint")?;
        validate_endpoint(endpoint)?;
        if self.isolated && endpoint == "http://127.0.0.1:8461/v1/systemone" {
            return Err("Isolated tests require a separate model endpoint".into());
        }
        let health = endpoint.replace("/v1/systemone", "/health");
        if Command::new("curl")
            .args(["-fsS", "--max-time", "1", &health])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(());
        }
        let mut process = self.chief_process.lock().unwrap();
        let running = process
            .as_mut()
            .is_some_and(|p| p.try_wait().ok().flatten().is_none());
        if !running {
            let binary = config["server"]
                .as_str()
                .ok_or("Missing local model server")?;
            let weights = config["weights"].as_str().ok_or("Missing model weights")?;
            if !std::path::Path::new(binary).is_file() || !std::path::Path::new(weights).is_file() {
                return Err("Install the decision runtime and weights, or configure an existing local /v1/systemone server".into());
            }
            let port = endpoint
                .trim_start_matches("http://127.0.0.1:")
                .split('/')
                .next()
                .unwrap();
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.data.join("chief.log"))
                .map_err(|e| e.to_string())?;
            let mut command = Command::new(binary);
            command
                .args([
                    "-m",
                    weights,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    port,
                    "-c",
                    "4096",
                    "-ngl",
                ])
                .arg(config["gpu_layers"].as_u64().unwrap_or(99).to_string())
                .args(["--parallel", "1", "--threads"])
                .arg(config["threads"].as_u64().unwrap_or(2).to_string())
                .stdin(Stdio::null())
                .stdout(log.try_clone().map_err(|e| e.to_string())?)
                .stderr(log);
            if config["gpu_layers"] == 0 {
                command.args(["--device", "none", "--no-op-offload", "--no-mmproj-offload"]);
            }
            if let Some(projection) = config["projection"].as_str().filter(|p| !p.is_empty()) {
                command.args(["--mmproj", projection]);
            }
            *process = Some(command.spawn().map_err(|e| e.to_string())?);
            drop(process);
            self.event(
                "chief",
                "Loading the local decision model",
                json!({"name":config["name"]}),
            );
        } else {
            drop(process);
        }
        for _ in 0..240 {
            if self.generation.load(Ordering::SeqCst) != generation {
                return Err("Command stopped while loading the model".into());
            }
            if Command::new("curl")
                .args(["-fsS", "--max-time", "1", &health])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err("Local chief did not become ready; see chief.log".into())
    }

    pub fn decide(&self, text: &str, generation: u64) -> Result<Value> {
        let settings = self.settings();
        let sessions = self.sessions()?;
        let (browser, desktop) = {
            let s = self.state.lock().unwrap();
            (s.browser.clone(), s.desktop.clone())
        };
        // Only explicitly grounded targets are candidates. The local model is
        // still the authority on whether the command requests each action.
        let mut questions = serde_json::Map::new();
        let mut actions = serde_json::Map::new();
        let mut add = |key: String, description: String, action: Value| {
            questions.insert(key.clone(), json!({"type":"noul","instructions":format!("Does the user request to {description}? Answer no for negated instructions, conversation, or missing context.")}));
            actions.insert(key, action);
        };
        let frozen = negated(text);
        let switching = [
            "switch",
            "go back",
            "return to",
            "select",
            "resume",
            "continue",
            "focus",
            "existing",
        ]
        .iter()
        .any(|word| mentioned(text, word));
        let launching = ["open", "launch", "start", "create", "new", "fresh"]
            .iter()
            .any(|word| mentioned(text, word));
        // Registered projects, not the settings file's seed list.
        let valid_project = project_for(text, &json!({"projects": self.projects()?})).is_ok();
        if !frozen && immediate_request(text) {
            if desktop["status"] == "connected" && mentioned(text, "window") {
                let observed = self.desktop_request(&json!({"type":"windows"}), generation)?;
                if observed["ok"] == false {
                    return Err("Native windows could not be observed".into());
                }
                let matched = observed["windows"]
                    .as_array()
                    .ok_or("Missing window handles")?
                    .iter()
                    .filter(|window| {
                        mentioned(text, window["name"].as_str().unwrap_or(""))
                            || mentioned(text, window["app"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>();
                if matched.len() == 1 && (switching || launching) {
                    let window = matched[0];
                    add(
                        format!("window:{}", window["id"]),
                        format!(
                            "focus the {} window",
                            window["app"].as_str().unwrap_or("named")
                        ),
                        json!({"kind":"desktop_action","action":{"type":"window","id":window["id"]}}),
                    );
                }
            }
            let profiles = settings["profiles"].as_array().ok_or("Missing profiles")?;
            let named = profiles
                .iter()
                .filter(|profile| {
                    mentioned(text, profile["name"].as_str().unwrap_or(""))
                        || mentioned(text, profile["id"].as_str().unwrap_or(""))
                })
                .collect::<Vec<_>>();
            let specific = named.iter().any(|p| p["id"] != "terminal");
            if launching && !switching && valid_project {
                for profile in named.iter().filter(|p| !specific || p["id"] != "terminal") {
                    let id = profile["id"].as_str().ok_or("Invalid profile ID")?;
                    // Use the user's literal registered alias, avoiding an
                    // expanded product name that changes a short command.
                    let alias = if mentioned(text, id) {
                        id
                    } else {
                        profile["name"].as_str().unwrap_or(id)
                    };
                    add(
                        format!("launch:{id}"),
                        format!("open or launch a new {alias} session"),
                        json!({"kind":"launch","profile":id}),
                    );
                }
            }
            if switching {
                let matched = sessions
                    .iter()
                    .filter(|s| {
                        s["running"] == true && mentioned(text, s["name"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>();
                if matched.len() == 1 {
                    let session = matched[0];
                    add(
                        format!("switch:{}", session["id"].as_str().unwrap()),
                        format!(
                            "switch to the existing {} session",
                            session["name"].as_str().unwrap()
                        ),
                        json!({"kind":"switch","id":session["id"]}),
                    );
                }
                // BUG-069: "continue/resume X" reopens the harness's native
                // conversation in a fresh pane rather than focusing the pane.
                let resuming = mentioned(text, "resume") || mentioned(text, "continue");
                if resuming {
                    for session in sessions.iter().filter(|s| {
                        s["native_id"].is_string()
                            && mentioned(text, s["name"].as_str().unwrap_or(""))
                    }) {
                        add(
                            format!("resume:{}", session["id"].as_str().unwrap()),
                            format!(
                                "resume the {} harness conversation in a new session",
                                session["name"].as_str().unwrap()
                            ),
                            json!({"kind":"resume","id":session["id"]}),
                        );
                    }
                }
            }
            let mut favorite_found = false;
            for (index, favorite) in settings["favorites"]
                .as_array()
                .ok_or("Missing favorites")?
                .iter()
                .enumerate()
            {
                if mentioned(text, favorite["name"].as_str().unwrap_or("")) && launching {
                    favorite_found = true;
                    add(
                        format!("favorite:{index}"),
                        format!("open {} in the browser", favorite["name"].as_str().unwrap()),
                        json!({"kind":"url","url":favorite["url"]}),
                    );
                }
            }
            if !favorite_found
                && (mentioned(text, "browser") || mentioned(text, "zen"))
                && ["open", "launch", "start", "bring up", "focus"]
                    .iter()
                    .any(|word| mentioned(text, word))
            {
                let alias = if mentioned(text, "zen") {
                    "Zen"
                } else {
                    "the browser"
                };
                add(
                    "browser".into(),
                    format!("open or bring up {alias}"),
                    json!({"kind":"browser"}),
                );
            }
            if (switching || mentioned(text, "attach"))
                && let Some(tabs) = browser["tabs"].as_array()
            {
                for tab in tabs {
                    if mentioned(text, tab["title"].as_str().unwrap_or("")) {
                        if mentioned(text, "attach") {
                            add(
                                format!("attach:{}", tab["id"]),
                                format!(
                                    "attach the {} browser tab for control",
                                    tab["title"].as_str().unwrap()
                                ),
                                json!({"kind":"browser_action","action":{"type":"attach","tab":tab["id"]}}),
                            );
                            continue;
                        }
                        add(
                            format!("tab:{}", tab["id"]),
                            format!(
                                "switch to the browser tab {}",
                                tab["title"].as_str().unwrap()
                            ),
                            json!({"kind":"browser_action","action":{"type":"tab","tab":tab["id"]}}),
                        );
                    }
                }
            }
            if mentioned(text, "click") || mentioned(text, "press") || mentioned(text, "activate") {
                if let Some(elements) = browser["elements"].as_array() {
                    for element in elements.iter().take(80) {
                        if mentioned(text, element["name"].as_str().unwrap_or("")) {
                            add(
                                format!("element:{}", element["ref"].as_str().unwrap_or("")),
                                format!("click the {} control", element["name"].as_str().unwrap()),
                                json!({"kind":"browser_action","action":{"type":"click","ref":element["ref"],"document":browser["document"],"tab":browser["attached"]}}),
                            );
                        }
                    }
                }
                if let Some(controls) = desktop["controls"].as_array() {
                    for control in controls.iter().take(100) {
                        if mentioned(text, control["name"].as_str().unwrap_or("")) {
                            add(
                                format!("desktop:{}", control["id"].as_str().unwrap_or("")),
                                format!(
                                    "activate {} on the desktop",
                                    control["name"].as_str().unwrap()
                                ),
                                json!({"kind":"desktop_action","action":{"type":"activate","id":control["id"],"revision":desktop["revision"]}}),
                            );
                        }
                    }
                }
            }
            if desktop["status"] == "connected" {
                for (key, phrase, action) in [
                    (
                        "scroll_down",
                        "scroll down",
                        json!({"type":"scroll","dy":3.0}),
                    ),
                    ("scroll_up", "scroll up", json!({"type":"scroll","dy":-3.0})),
                    ("escape", "press escape", json!({"type":"key","key":"ESC"})),
                    ("enter", "press enter", json!({"type":"key","key":"ENTER"})),
                    (
                        "next_window",
                        "next window",
                        json!({"type":"chord","keys":["ALT","TAB"]}),
                    ),
                ] {
                    if mentioned(text, phrase) {
                        add(
                            key.into(),
                            format!("{phrase} on the connected desktop"),
                            json!({"kind":"desktop_action","action":action}),
                        );
                    }
                }
            }
        }
        // Nothing grounded to act on: the answer is conversation, and asking
        // the local model would only add latency (or fail when it is absent).
        if actions.is_empty() || frozen {
            return Ok(json!({"selected":"noop","action":{"kind":"noop"},"elapsed_ms":0,
                "raw":null,"skipped":true}));
        }
        self.ensure_chief(generation)?;
        questions.insert("noop".into(),json!({"type":"noul","instructions":"Is this a greeting, conversation, cancellation, negated request, or request to keep the current state, requiring no action?"}));
        // Plain text and bounded questions keep session identifiers, browser
        // content and screenshots out of ordinary launch/switch inference.
        let request = json!({"state":format!("User command: {text}\nNavi is the local terminal station. Claude is Claude Code. Zen is the user's web browser. Project names identify registered working directories."),"questions":questions});
        let endpoint = settings["model"]["endpoint"].as_str().unwrap();
        let started = now();
        // An unlinked private file bounds the response without pipe deadlocks
        // and lets Stop interrupt the HTTP client while inference is pending.
        let response_path = self.runtime.join(format!("decision-{}.json", crate::id()));
        let mut response_file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&response_path)
            .map_err(|error| error.to_string())?;
        std::fs::remove_file(response_path).map_err(|error| error.to_string())?;
        let mut child = Command::new("curl")
            .args([
                "-fsS",
                "--max-time",
                "30",
                "--max-filesize",
                "4194304",
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                "@-",
                endpoint,
            ])
            .stdin(Stdio::piped())
            .stdout(
                response_file
                    .try_clone()
                    .map_err(|error| error.to_string())?,
            )
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        let status = loop {
            if self.generation.load(Ordering::SeqCst) != generation {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Command stopped during decision inference".into());
            }
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if !status.success() {
            return Err("Decision inference failed; check the local model server".into());
        }
        response_file
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        let mut payload = Vec::new();
        response_file
            .take((crate::MAX_MESSAGE + 1) as u64)
            .read_to_end(&mut payload)
            .map_err(|error| error.to_string())?;
        if payload.len() > crate::MAX_MESSAGE {
            return Err("Decision response exceeded the size limit".into());
        }
        let raw: Value =
            serde_json::from_slice(&payload).map_err(|e| format!("Invalid model response: {e}"))?;
        let minimum = settings["model"]["minimum_probability"]
            .as_f64()
            .unwrap_or(0.65);
        let mut scored = actions
            .keys()
            .filter_map(|key| {
                raw["answers"][key]["noul"]
                    .as_f64()
                    .filter(|p| p.is_finite() && *p >= minimum && *p <= 1.0)
                    .map(|p| (key, p))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let noop = raw["answers"]["noop"]["noul"].as_f64().unwrap_or(0.0);
        let selected = if frozen {
            "noop"
        } else if scored.first().is_some_and(|first| {
            first.1 > noop
                && scored
                    .get(1)
                    .is_none_or(|second| first.1 - second.1 >= 0.15)
        }) {
            scored[0].0.as_str()
        } else if noop >= minimum {
            "noop"
        } else {
            "clarify"
        };
        let action = actions
            .get(selected)
            .cloned()
            .unwrap_or(json!({"kind":selected}));
        Ok(json!({"selected":selected,"action":action,"elapsed_ms":now()-started,"raw":raw}))
    }

    // Persona switches are deterministic voice commands, never model output:
    // "switch to navi mode", "be concise", "use the navi persona". Returns
    // the switch outcome so dispatch can answer without a decision round-trip.
    // Load the decision model in the background so the first spoken command
    // does not pay the multi-second model start. Failures surface on the next
    // real command, not as startup noise.
    pub fn warm_chief(&self) {
        if self.isolated {
            return;
        }
        let generation = self.generation.load(Ordering::SeqCst);
        let _ = self.ensure_chief(generation);
    }

    pub fn persona_switch(&self, text: &str) -> Result<Option<PersonaSwitch>> {
        let Some(target) = persona_target(text) else {
            return Ok(None);
        };
        let names = self.persona_names()?;
        let matched = names
            .iter()
            .find(|name| {
                target == name.as_str()
                    || target
                        .strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.starts_with(' '))
            })
            .cloned()
            .or(self.persona_resolve(&target)?);
        let Some(name) = matched else {
            return Ok(None);
        };
        let mut settings = self.settings();
        // Switching to the persona already in use changes nothing, so the
        // words are left to the commands: "switch to Navi" names the project.
        if settings["persona"].as_str() == Some(name.as_str()) {
            return Ok(None);
        }
        settings["persona"] = json!(name);
        self.save_settings(settings, false)?;
        self.event("persona", "Persona switched", json!({"persona":name}));
        Ok(Some(PersonaSwitch {
            name: name.clone(),
            reply: format!("Persona switched to {name}."),
        }))
    }

    fn persona_resolve(&self, target: &str) -> Result<Option<String>> {
        let output = Command::new(self.python())
            .arg(self.root.join("workers/persona.py"))
            .arg("--resolve")
            .arg(target)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(name).filter(|name| !name.is_empty()))
    }

    fn persona_names(&self) -> Result<Vec<String>> {
        let output = Command::new(self.python())
            .arg(self.root.join("workers/persona.py"))
            .arg("--names")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect())
    }

    /// One assistant request: which project directory the gateway works in and
    /// the ≤600-byte project context prepended to the prompt (BUG-004/088).
    fn assistant_turn(&self, params: &Value) -> Result<AssistantTurn> {
        let project = self.assistant_project(params["project_id"].as_str())?;
        let id = project["id"].as_str().unwrap_or_default().to_string();
        let path = project["path"].as_str().unwrap_or_default().to_string();
        if path.is_empty() {
            return Err("Project has no directory".into());
        }
        let context = self.context_block(&project);
        let attachments = self.prepare_attachments(&params["attachments"])?;
        Ok(AssistantTurn { id, path, context, attachments })
    }

    /// "Which one: Codex in Navi, Codex in acme-api?" — remembered for a
    /// minute so the next command can be just the answer.
    fn ask_which(&self, action: &Value, many: &[&Value], labels: &HashMap<String, String>) -> Result<String> {
        let names = choice_labels(many, labels, &self.projects()?);
        let choices: Vec<Value> = many
            .iter()
            .zip(&names)
            .map(|(session, label)| json!({"id":session["id"],"name":session["name"],"label":label}))
            .collect();
        *self.pending_choice.lock().unwrap() =
            Some(json!({"time":now(),"action":action,"choices":choices}));
        Ok(format!("Which one: {}?", names.join(", ")))
    }

    /// Whether the command's project has a part titled `spoken` (loosely).
    fn names_part(&self, clause: &str, spoken: &str) -> bool {
        let Ok(project) = self.command_project(clause, &Value::Null) else {
            return false;
        };
        let wanted = loose_title(spoken.strip_suffix(" part").unwrap_or(spoken));
        self.graph_get(project["id"].as_str()).is_ok_and(|graph| {
            graph["aspects"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|aspect| loose_title(aspect["title"].as_str().unwrap_or("")) == wanted)
        })
    }

    /// Whether the command's project has a task or part named like `spoken`.
    fn names_task_or_part(&self, clause: &str, spoken: &str) -> bool {
        let Ok(project) = self.command_project(clause, &Value::Null) else {
            return false;
        };
        let Ok(graph) = self.graph_get(project["id"].as_str()) else {
            return false;
        };
        let wanted = loose_title(spoken);
        let aspects = graph["aspects"].as_array().into_iter().flatten();
        aspects
            .clone()
            .flat_map(|aspect| aspect["items"].as_array().into_iter().flatten())
            .chain(graph["unassigned"].as_array().into_iter().flatten())
            .chain(aspects)
            .any(|row| {
                let title = loose_title(row["title"].as_str().unwrap_or(""));
                // Whole words: a part named "UI" is not in "the build".
                !title.is_empty() && (mentioned(&title, &wanted) || mentioned(&wanted, &title))
            })
    }

    /// "Tell Codex to …" while Codex is busy: hold the prompt until the pane
    /// is free (up to ten minutes of waiting), then type it. Several held
    /// prompts for one session go one at a time, in order. Stop drops them.
    fn deliver_when_free(&self, id: &str, prompt: &str, label: &str) -> Result<()> {
        {
            let mut held = self.held_prompts.lock().unwrap();
            let queue = held.entry(id.to_string()).or_default();
            queue.push(prompt.to_string());
            // A waiter already runs for this session; it sends this one next.
            if queue.len() > 1 {
                return Ok(());
            }
        }
        let app = self.me.upgrade().ok_or("Navi is shutting down")?;
        let (id, label) = (id.to_string(), label.to_string());
        let generation = self.generation.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            let project = || {
                app.sessions().ok().and_then(|sessions| {
                    sessions.iter().find(|session| session["id"] == id.as_str())?["project_id"]
                        .as_str()
                        .map(str::to_string)
                })
            };
            let mut waiting_since = now();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let session = app
                    .sessions()
                    .ok()
                    .and_then(|sessions| sessions.into_iter().find(|session| session["id"] == id.as_str()));
                let failure = if app.generation.load(Ordering::SeqCst) != generation {
                    Some("Stop was pressed".to_string())
                } else if now().saturating_sub(waiting_since) > 600_000 {
                    Some("it stayed busy for ten minutes".to_string())
                } else {
                    match &session {
                        None => Some("the session is gone".to_string()),
                        Some(session) if session["running"] != true => Some(format!("{label} closed")),
                        _ => None,
                    }
                };
                if let Some(reason) = failure {
                    let dropped = app.held_prompts.lock().unwrap().remove(&id).unwrap_or_default();
                    let project = project();
                    for prompt in dropped {
                        app.event_for(
                            project.as_deref(),
                            Some(&id),
                            "warning",
                            &format!("Did not send “{prompt}” to {label}: {reason}."),
                            json!({"id":id}),
                        );
                    }
                    return;
                }
                if session.is_some_and(|session| {
                    matches!(session["state"].as_str(), Some("busy" | "starting" | "created"))
                }) {
                    continue;
                }
                let Some(prompt) = app.held_prompts.lock().unwrap().get(&id).and_then(|queue| queue.first().cloned())
                else {
                    return;
                };
                let (kind, text) = match app.inject(&id, &prompt, true) {
                    Ok(_) => ("delivered", format!("Sent to {label}: “{prompt}”")),
                    Err(error) => ("warning", format!("Did not send “{prompt}” to {label}: {error}")),
                };
                app.event_for(project().as_deref(), Some(&id), kind, &text, json!({"id":id,"name":label}));
                {
                    let mut held = app.held_prompts.lock().unwrap();
                    let Some(queue) = held.get_mut(&id) else { return };
                    queue.remove(0);
                    if queue.is_empty() {
                        held.remove(&id);
                        return;
                    }
                }
                // The next prompt waits for this one to take the pane.
                std::thread::sleep(Duration::from_secs(3));
                waiting_since = now();
            }
        });
        Ok(())
    }

    /// The choices of a "Which one" this command just asked, for the chat to
    /// offer as buttons (a pending question from before was taken already).
    /// Remember a progress change for "undo": the user signals it added and
    /// the task state it replaced.
    fn remember_change(&self, project_id: &str, before: i64, task: Option<(&str, &str)>, label: &str) -> Result<()> {
        let until = self.last_signal_id()?;
        *self.last_change.lock().unwrap() = Some(json!({"time":now(),"project_id":project_id,"after":before,
            "until":until,"task_id":task.map(|task| task.0),"task_state":task.map(|task| task.1),"label":label}));
        Ok(())
    }

    /// "What's left", "the next task": the prompt that hands a harness the
    /// open work Navi records for `project_id`, and how the reply names it.
    /// Any other prompt is sent as spoken (`None`).
    fn work_prompt_for(&self, prompt: &str, project_id: Option<&str>) -> Result<Option<(String, String)>> {
        let Some(next_only) = work_request(prompt) else {
            return Ok(None);
        };
        let graph = self.graph_get(project_id)?;
        let name = graph["project"]["name"].as_str().unwrap_or("the project").to_string();
        let summary = match (next_only, next_task(&graph).1) {
            (false, _) => format!("the open work for {name}"),
            (true, Some((title, part))) if !title.is_empty() => format!("the next task, {}", named_task(&title, &part)),
            (true, Some((_, part))) => format!("the {part} part, which is furthest behind"),
            (true, None) => format!("a request to find what {name} is missing"),
        };
        Ok(Some((work_prompt(&graph, next_only), summary)))
    }

    /// The session "it" means: one this command just opened ("open a new
    /// Codex and tell it to …"), else the voice target.
    fn pronoun_session(&self) -> Option<String> {
        self.pronoun_target
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.settings()["active_session"].as_str().map(str::to_string))
    }

    fn choice_extra(&self) -> Value {
        match self.pending_choice.lock().unwrap().as_ref() {
            Some(pending) => {
                let labels: Vec<Value> =
                    pending["choices"].as_array().into_iter().flatten().map(|choice| choice["label"].clone()).collect();
                json!({"choices": labels})
            }
            None => json!({}),
        }
    }

    /// Voice sends no project: the one the station last reported, else the
    /// project of the session voice targets.
    fn station_project(&self, projects: &[Value]) -> Option<Value> {
        let current = self.current_project.lock().unwrap().clone();
        let active = self.settings()["active_session"].as_str().and_then(|id| {
            self.sessions().ok()?.into_iter().find(|session| session["id"] == id)?["project_id"]
                .as_str()
                .map(str::to_string)
        });
        [current, active]
            .into_iter()
            .flatten()
            .find_map(|id| projects.iter().find(|project| project["id"] == id.as_str()).cloned())
    }

    pub(crate) fn assistant_project(&self, reference: Option<&str>) -> Result<Value> {
        match reference.filter(|value| !value.trim().is_empty()) {
            Some(reference) => self.resolve_project(reference),
            None => {
                let projects = self.projects()?;
                if let Some(project) = self.station_project(&projects) {
                    return Ok(project);
                }
                if projects.len() != 1 {
                    return Err(format!(
                        "Pass project_id; {} projects are registered",
                        projects.len()
                    ));
                }
                Ok(projects[0].clone())
            }
        }
    }

    /// Resolve which assistant session a project owns (BUG-090). The switcher
    /// asks before the next turn so the UI can report the scope, and the answer
    /// is the directory the turn will run in plus the session id recorded for
    /// it. The live turn's `assistant_dir` is deliberately not overwritten: an
    /// interrupt or a permission reply sent after a switch must still reach the
    /// turn that is actually running.
    pub(crate) fn assistant_scope(&self, p: &Value) -> Result<Value> {
        let project = self.assistant_project(p["project_id"].as_str())?;
        let id = project["id"].as_str().unwrap_or_default().to_string();
        let directory = project["path"].as_str().unwrap_or_default().to_string();
        if directory.is_empty() {
            return Err("Project has no directory".into());
        }
        *self.current_project.lock().unwrap() = Some(id.clone());
        let session: Option<String> = {
            let state = self.state.lock().unwrap();
            state
                .db
                .query_row(
                    "SELECT session_id FROM assistant_sessions WHERE project_id=?1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
        };
        Ok(json!({"project_id":id,"directory":directory,"session":session}))
    }

    /// Host-generated project context prepended to every assistant prompt,
    /// capped at 600 bytes: project, path, branch@sha, aspect percentages with
    /// verified flags, open task count, last activity (BUG-088).
    fn context_block(&self, project: &Value) -> String {
        let name = project["name"].as_str().unwrap_or("project");
        let path = project["path"].as_str().unwrap_or("");
        let id = project["id"].as_str().unwrap_or("");
        let mut lines = vec![format!("[project {name} · {path}]")];
        let branch = store::git(path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty() && text != "HEAD");
        let sha = store::git(path, &["rev-parse", "--short=12", "HEAD"])
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        match (branch, sha) {
            (Some(branch), Some(sha)) => lines.push(format!("branch {branch}@{sha}")),
            (None, Some(sha)) => lines.push(format!("detached @{sha}")),
            _ => {}
        }
        if let Ok(graph) = self.graph_get(Some(id)) {
            let aspects = graph["aspects"]
                .as_array()
                .map(|aspects| {
                    aspects
                        .iter()
                        .map(|aspect| {
                            let title = aspect["title"].as_str().unwrap_or("aspect");
                            match aspect["percent"].as_f64() {
                                Some(percent) => {
                                    let verified = aspect["verified"].as_f64().unwrap_or(0.0);
                                    if verified > 0.0 {
                                        format!("{title} {percent}% (verified {verified}%)")
                                    } else {
                                        format!("{title} {percent}%")
                                    }
                                }
                                None => format!("{title} —"),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if !aspects.is_empty() {
                lines.push(format!("aspects: {aspects}"));
            }
            let tasks = &graph["tasks"];
            let open = tasks["todo"].as_u64().unwrap_or(0)
                + tasks["in_progress"].as_u64().unwrap_or(0)
                + tasks["blocked"].as_u64().unwrap_or(0);
            lines.push(format!("open tasks: {open}"));
        }
        if let Ok(Some(at)) = self.project_last_activity(id) {
            lines.push(format!("last activity: {}", relative_time(at)));
        }
        cap_block(&lines.join("\n"))
    }

    /// Decode attachment params into gateway --attachment file paths. `data`
    /// payloads land in private runtime files; `path` payloads are checked and
    /// passed through. Limits: 4 files, 2 MiB decoded each.
    fn prepare_attachments(&self, params: &Value) -> Result<Vec<String>> {
        let Some(items) = params.as_array() else {
            return Ok(Vec::new());
        };
        validate_attachments(params)?;
        let mut paths = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if let Some(path) = item["path"].as_str() {
                let meta = std::fs::metadata(path).map_err(|error| {
                    format!("Attachment {} is not readable: {error}", item["name"].as_str().unwrap_or(path))
                })?;
                if !meta.is_file() {
                    return Err("Attachment path must be a file".into());
                }
                paths.push(path.to_string());
                continue;
            }
            let data = item["data"].as_str().unwrap_or_default();
            let bytes = b64_decode(data).ok_or("Attachment data is not valid base64")?;
            let name = item["name"].as_str().unwrap_or("attachment");
            let safe: String = name
                .chars()
                .map(|c| if matches!(c, '/' | '\\') { '_' } else { c })
                .collect();
            let file = self
                .runtime
                .join(format!("attach-{}-{index}-{safe}", crate::id()));
            std::fs::write(&file, &bytes).map_err(|error| error.to_string())?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
            paths.push(file.to_string_lossy().into_owned());
        }
        Ok(paths)
    }

    /// Interrupt the server-side turn for the current assistant directory,
    /// then kill the gateway child. Never kill without interrupting (BUG-007).
    fn abort_gateway(&self, directory: Option<&str>, child: &mut std::process::Child) -> bool {
        let mut args = vec!["interrupt".to_string()];
        if let Some(directory) = directory.filter(|dir| !dir.is_empty()) {
            args.push("--directory".into());
            args.push(directory.to_string());
        }
        let interrupted = self
            .gateway_op(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .ok()
            .and_then(|value| value["interrupted"].as_bool())
            .unwrap_or(false);
        if !interrupted {
            self.event_for(
                None,
                None,
                "warning",
                "Assistant turn may still be running: OpenCode did not confirm the interrupt",
                json!({"interrupted":false}),
            );
        }
        let _ = child.kill();
        let _ = child.wait();
        interrupted
    }

    /// Stream one OpenCode gateway turn: every gateway line becomes a daemon
    /// event (reply_delta, reasoning/thinking, tool_use, approval, warning)
    /// and the done record is returned. Stop interrupts the OpenCode turn
    /// before the child dies; gateway failures return Err (BUG-001/007/014).
    fn stream_gateway(
        &self,
        args: &[String],
        generation: u64,
        persona: &str,
        project_id: Option<&str>,
        directory: Option<&str>,
    ) -> std::result::Result<Value, GatewayFailure> {
        let mut child = Command::new(self.python())
            .arg(self.root.join("workers/opencode_gateway.py"))
            .args(args)
            .env("NAVI_RUNTIME_DIR", &self.runtime)
            .env("NAVI_DATA_DIR", &self.data)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| GatewayFailure::new(error.to_string(), false))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GatewayFailure::new("Gateway has no stdout", false))?;
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = now() + 190_000;
        let turn_id = self.begin_turn();
        let mut turn = GatewayTurn::default();
        let mut done: Option<Value> = None;
        // Characters accumulated since the last flush; a silent stream still
        // publishes them from the receive timeout below.
        let mut unflushed = 0_usize;
        let mut flushed_at = now();
        loop {
            if self.generation.load(Ordering::SeqCst) != generation {
                let interrupted = self.abort_gateway(directory, &mut child);
                self.turn_finish(&turn_id, &turn.accumulated);
                return Err(GatewayFailure::new("stopped", interrupted));
            }
            if now() > deadline {
                let interrupted = self.abort_gateway(directory, &mut child);
                self.turn_finish(&turn_id, &turn.accumulated);
                return Err(GatewayFailure::new(
                    format!("The assistant took too long to answer (interrupted: {interrupted})"),
                    interrupted,
                ));
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => {
                    let Some(mut event) = map_gateway_line(&line) else {
                        continue;
                    };
                    if !turn.record(&mut event) {
                        continue;
                    }
                    // Text must reach the client before the tool row that
                    // follows it, so a pending window is flushed first.
                    if !matches!(event, GatewayLine::Delta(_)) && unflushed > 0 {
                        self.turn_flush(&turn_id, &turn.accumulated, persona, project_id);
                        unflushed = 0;
                        flushed_at = now();
                    }
                    match event {
                        GatewayLine::Delta(text) => {
                            unflushed += text.chars().count();
                            if unflushed >= DELTA_FLUSH_CHARS
                                || now().saturating_sub(flushed_at) >= DELTA_FLUSH_MS
                            {
                                self.turn_flush(&turn_id, &turn.accumulated, persona, project_id);
                                unflushed = 0;
                                flushed_at = now();
                            }
                        }
                        GatewayLine::Reasoning(text, first) => {
                            if first {
                                self.event_for(
                                    project_id,
                                    None,
                                    "thinking",
                                    "The assistant is reasoning",
                                    json!({"persona":persona}),
                                );
                            }
                            self.event_for(
                                project_id,
                                None,
                                "reasoning",
                                "Assistant reasoning",
                                json!({"text":text,"persona":persona}),
                            );
                        }
                        GatewayLine::ToolStatus {
                            id,
                            name,
                            input,
                            progress,
                        } => {
                            self.event_for(
                                project_id,
                                None,
                                "tool_use",
                                &format!("{} · running", name.as_deref().unwrap_or("tool")),
                                json!({"id":id,"name":name,"status":"running",
                                       "input":input,"progress":progress}),
                            );
                        }
                        GatewayLine::ToolFinal {
                            id,
                            name,
                            status,
                            input,
                            output,
                            exit,
                        } => {
                            self.event_for(
                                project_id,
                                None,
                                "tool_use",
                                &format!("{} · {}", name.as_deref().unwrap_or("tool"), status),
                                json!({"id":id,"name":name,"status":status,"input":input,
                                       "output":output,"exit":exit,"final":true}),
                            );
                        }
                        GatewayLine::Permission { id, tool, input } => {
                            self.event_for(
                                project_id,
                                None,
                                "approval",
                                &format!("{} needs approval", tool.as_deref().unwrap_or("A tool")),
                                json!({"id":id,"tool":tool,"input":input}),
                            );
                        }
                        GatewayLine::Warning(text) => {
                            self.event_for(
                                project_id,
                                None,
                                "warning",
                                &text,
                                json!({"text":text}),
                            );
                        }
                        GatewayLine::Error(text) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            self.turn_finish(&turn_id, &turn.accumulated);
                            return Err(GatewayFailure::new(text, false));
                        }
                        GatewayLine::Done(value) => done = Some(*value),
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // A stalled stream must still publish what it already has.
                    if unflushed > 0 && now().saturating_sub(flushed_at) >= DELTA_FLUSH_MS {
                        self.turn_flush(&turn_id, &turn.accumulated, persona, project_id);
                        unflushed = 0;
                        flushed_at = now();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if let Some(value) = done {
                let _ = child.wait();
                return Ok(self.completed_turn(&turn_id, value, &mut turn));
            }
        }
        let status = child
            .wait()
            .map_err(|error| GatewayFailure::new(error.to_string(), false))?;
        if let Some(value) = done {
            return Ok(self.completed_turn(&turn_id, value, &mut turn));
        }
        self.turn_finish(&turn_id, &turn.accumulated);
        Err(GatewayFailure::new(
            format!("Assistant exited without a reply ({status})"),
            false,
        ))
    }

    /// Finish a gateway record: the turn id travels with the reply event and the
    /// final text becomes the authoritative `turn_state` (BUG-098).
    fn completed_turn(&self, turn_id: &str, value: Value, turn: &mut GatewayTurn) -> Value {
        let mut done = self.finish_turn(value, turn);
        let text = done["text"].as_str().unwrap_or_default().to_string();
        done["turn"] = json!(turn_id);
        self.turn_finish(turn_id, &text);
        done
    }

    /// The reply text is the concatenated deltas: a done record with empty or
    /// missing text falls back to what streamed (BUG-001 contract).
    fn finish_turn(&self, mut done: Value, turn: &mut GatewayTurn) -> Value {
        if (!done["text"].is_string() || done["text"].as_str().unwrap_or("").is_empty())
            && !turn.accumulated.is_empty()
        {
            done["text"] = json!(std::mem::take(&mut turn.accumulated));
        }
        done
    }

    pub fn gateway_op(&self, args: &[&str]) -> Result<Value> {
        // Bound the gateway call: an unreachable OpenCode must fail fast, not
        // pin the UI request that asked for its catalog.
        let mut child = Command::new(self.python())
            .arg(self.root.join("workers/opencode_gateway.py"))
            .args(args)
            .env("NAVI_RUNTIME_DIR", &self.runtime)
            .env("NAVI_DATA_DIR", &self.data)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut stdout = child.stdout.take().unwrap_or_else(|| unreachable!());
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stdout.read_to_end(&mut buffer);
            let _ = sender.send(buffer);
        });
        let output = match receiver.recv_timeout(Duration::from_secs(15)) {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("OpenCode did not answer in time".into());
            }
        };
        let status = child.wait().map_err(|error| error.to_string())?;
        if !status.success() {
            return Err("OpenCode is not reachable".into());
        }
        for line in String::from_utf8_lossy(&output).lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                return Ok(value);
            }
        }
        Err("OpenCode returned no data".into())
    }

    /// Interrupt the server-side assistant turn (cancel path, BUG-007). The
    /// most recent assistant directory scopes the interrupt to that session.
    pub fn gateway_interrupt(&self) -> bool {
        let directory = self.assistant_dir.lock().unwrap().clone();
        let mut args = vec!["interrupt"];
        if let Some(directory) = directory.as_deref() {
            args.push("--directory");
            args.push(directory);
        }
        self.gateway_op(&args)
            .ok()
            .and_then(|value| value["interrupted"].as_bool())
            .unwrap_or(false)
    }

    /// POST a permission decision for a gated tool (BUG-006).
    pub fn permission_reply(&self, permission_id: &str, decision: &str) -> Result<Value> {
        if !matches!(decision, "allow" | "deny") {
            return Err("Permission decision must be allow or deny".into());
        }
        if permission_id.is_empty() || permission_id.chars().any(char::is_control) {
            return Err("Invalid permission id".into());
        }
        let directory = self.assistant_dir.lock().unwrap().clone();
        let mut args = vec![
            "permission",
            "--id",
            permission_id,
            "--decision",
            decision,
        ];
        if let Some(directory) = directory.as_deref() {
            args.push("--directory");
            args.push(directory);
        }
        // BUG-006: a refusal is exit 0 with `{replied:false,error}` on stdout,
        // so the payload itself has to carry the failure to the UI.
        let reply = self.gateway_op(&args)?;
        let refusal = reply["error"].as_str().unwrap_or_default().trim();
        if reply["replied"].as_bool() != Some(true) || !refusal.is_empty() {
            return Err(if refusal.is_empty() {
                "OpenCode did not accept the permission decision".into()
            } else {
                refusal.to_string()
            });
        }
        Ok(reply)
    }

    /// Run a validated command/slash request under the command lock. Queued
    /// items call this from the drain thread; both paths resolve the project,
    /// emit `working` once, and reply through events (BUG-016/018).
    pub fn run_request(&self, params: &Value, generation: u64) -> Result<Value> {
        let request = params["request_id"].as_str().filter(|id| !id.is_empty());
        crate::with_request(request, || {
            let text = params["text"].as_str().unwrap_or_default();
            if text.trim_start().starts_with('/') && navi_slash(text).is_none() {
                return self.run_slash(text.trim(), params, generation);
            }
            self.run_command(text, params, false, generation)
        })
    }

    /// Take the command lock when free; otherwise enqueue (bounded at 4) and
    /// let the drain thread run it later (BUG-016/018).
    pub fn run_or_queue(&self, params: &Value, generation: u64) -> Result<Value> {
        if self.command_queue.lock().unwrap().is_empty()
            && let Ok(lock) = self.command_lock.try_lock()
        {
            let result = self.run_request(params, generation);
            drop(lock);
            return result;
        }
        let mut queue = self.command_queue.lock().unwrap();
        if queue.len() >= 4 {
            return Err("The assistant queue is full; cancel or wait for a reply".into());
        }
        queue.push_back(params.clone());
        let position = queue.len();
        self.event(
            "queued",
            "Command queued behind the running turn",
            json!({"position":position,"text":params["text"],"request_id":params["request_id"]}),
        );
        Ok(json!({"queued":true,"position":position}))
    }

    /// Drain thread: pops queued commands under the command lock until empty.
    /// Cancel clears the queue; each item re-reads the generation so a turn
    /// queued before Stop never runs the stale generation.
    pub fn drain_commands(self: &std::sync::Arc<Self>) {
        loop {
            if self.command_queue.lock().unwrap().is_empty() {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
            let Ok(_lock) = self.command_lock.try_lock() else {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            };
            loop {
                let params = self.command_queue.lock().unwrap().pop_front();
                let Some(params) = params else { break };
                let generation = self.generation.load(Ordering::SeqCst);
                if let Err(error) = self.run_request(&params, generation) {
                    self.event(
                        "error",
                        &error,
                        json!({"op":"command","request_id":params["request_id"]}),
                    );
                }
            }
        }
    }

    fn assistant_target(&self) -> (String, String, String) {
        let settings = self.settings();
        let persona = settings["persona"].as_str().unwrap_or("navi").to_string();
        let model = settings["assistant"]["model"]
            .as_str()
            .unwrap_or(crate::DEFAULT_ASSISTANT_MODEL)
            .to_string();
        let agent = settings["assistant"]["agent"]
            .as_str()
            .unwrap_or(crate::DEFAULT_ASSISTANT_AGENT)
            .to_string();
        (persona, model, agent)
    }

    /// Ask OpenCode through the gateway and return the assistant's reply.
    /// Gateway failures are Err, never synthesized reply text (BUG-014); a
    /// stopped turn is a reply marked `stopped`/`interrupted` (BUG-007).
    fn converse(&self, text: &str, turn: &AssistantTurn, generation: u64) -> Result<Value> {
        let (persona, model, agent) = self.assistant_target();
        let prompt = if turn.context.is_empty() {
            text.to_string()
        } else {
            format!("{}\n\n{}", turn.context, text)
        };
        let mut args = vec![
            "ask".to_string(),
            prompt,
            "--stream".to_string(),
            "--model".to_string(),
            model.clone(),
            "--agent".to_string(),
            agent.clone(),
            "--directory".to_string(),
            turn.path.clone(),
        ];
        for attachment in &turn.attachments {
            args.push("--attachment".into());
            args.push(attachment.clone());
        }
        *self.assistant_dir.lock().unwrap() = Some(turn.path.clone());
        let result = self.stream_gateway(
            &args,
            generation,
            &persona,
            Some(turn.id.as_str()),
            Some(turn.path.as_str()),
        );
        // Remove only runtime scratch files we decoded; user paths stay.
        let scratch = self.runtime.join("attach-").to_string_lossy().into_owned();
        for path in turn.attachments.iter().filter(|p| p.starts_with(&scratch)) {
            let _ = std::fs::remove_file(path);
        }
        match result {
            Ok(done) => {
                if let Some(session) = done["session"].as_str() {
                    let model_ref = done["actual_model"]
                        .as_object()
                        .map(|m| {
                            match (m.get("providerID").and_then(Value::as_str),
                                   m.get("id").and_then(Value::as_str)) {
                                (Some(provider), Some(id)) => format!("{provider}/{id}"),
                                (_, Some(id)) => id.to_string(),
                                _ => model.clone(),
                            }
                        })
                        .unwrap_or_else(|| model.clone());
                    let agent_ref = done["actual_agent"]
                        .as_str()
                        .unwrap_or(agent.as_str())
                        .to_string();
                    let _ = self.record_assistant_session(
                        turn.id.as_str(), session, &model_ref, &agent_ref,
                    );
                }
                // The assistant reports progress as PROGRESS lines; recording
                // them here links every chat answer to the project graph.
                let recorded = done["text"]
                    .as_str()
                    .map(|text| self.record_progress_lines(&turn.id, text).unwrap_or(0))
                    .unwrap_or(0);
                let reply = json!({"text":done["text"],"persona":persona,
                    "source":done["source"],"session":done["session"],
                    "usage":done["usage"],"session_usage":done["session_usage"],
                    "finish":done["finish"],"actual_model":done["actual_model"],
                    "actual_agent":done["actual_agent"],"warnings":done["warnings"],
                    "attachments":done["attachments"],"project_id":turn.id,
                    "turn":done["turn"],"progress_recorded":recorded});
                self.event_for(
                    Some(turn.id.as_str()),
                    done["session"].as_str(),
                    "reply",
                    reply["text"].as_str().unwrap_or("No reply"),
                    reply.clone(),
                );
                Ok(reply)
            }
            Err(failure) if failure.stopped => {
                let reply = json!({"text":"Conversation stopped.","persona":persona,
                    "source":"stopped","stopped":true,"interrupted":failure.interrupted});
                self.event_for(
                    Some(turn.id.as_str()),
                    None,
                    "reply",
                    "Conversation stopped.",
                    reply.clone(),
                );
                Ok(reply)
            }
            Err(failure) => Err(failure.message),
        }
    }

    /// Slash messages are OpenCode commands, not chief decisions. They join the
    /// same queue/lock as commands (BUG-018) and scope to the same project.
    pub fn run_slash(&self, text: &str, params: &Value, generation: u64) -> Result<Value> {
        let turn = self.assistant_turn(params)?;
        let (persona, model, agent) = self.assistant_target();
        self.event_for(
            Some(turn.id.as_str()),
            None,
            "working",
            "Running an OpenCode command",
            json!({"text":text}),
        );
        let args = vec![
            "slash".to_string(),
            text.to_string(),
            "--stream".to_string(),
            "--model".to_string(),
            model.clone(),
            "--agent".to_string(),
            agent.clone(),
            "--directory".to_string(),
            turn.path.clone(),
        ];
        *self.assistant_dir.lock().unwrap() = Some(turn.path.clone());
        match self.stream_gateway(
            &args,
            generation,
            &persona,
            Some(turn.id.as_str()),
            Some(turn.path.as_str()),
        ) {
            Ok(done) => {
                if let Some(session) = done["session"].as_str() {
                    let _ = self.record_assistant_session(
                        turn.id.as_str(), session, &model, &agent,
                    );
                }
                let reply = json!({"text":done["text"],"persona":persona,
                    "source":done["source"],"session":done["session"],
                    "usage":done["usage"],"session_usage":done["session_usage"],
                    "finish":done["finish"],"actual_model":done["actual_model"],
                    "actual_agent":done["actual_agent"],"warnings":done["warnings"],
                    "turn":done["turn"]});
                self.event_for(
                    Some(turn.id.as_str()),
                    done["session"].as_str(),
                    "reply",
                    reply["text"].as_str().unwrap_or("No reply"),
                    reply.clone(),
                );
                Ok(json!({"trace":[],"dry_run":false,"reply":reply}))
            }
            Err(failure) if failure.stopped => Ok(json!({"trace":[],"dry_run":false,
                "reply":{"text":"Conversation stopped.","persona":persona,
                         "source":"stopped","stopped":true,"interrupted":failure.interrupted},
                "stopped":true,"interrupted":failure.interrupted})),
            Err(failure) => Err(failure.message),
        }
    }

    /// Whether Navi's own assistant could answer now: OpenCode is reachable, or
    /// the gateway would find the CLI to start it. A failed probe is unavailable.
    pub(crate) fn assistant_available(&self) -> bool {
        self.gateway_op(&["status"])
            .is_ok_and(|status| status["reachable"] == true || status["spawnable"] == true)
    }

    /// Background assessment by Navi's own assistant when no harness runs in
    /// the project. It waits behind any running turn like a queued command;
    /// `converse` records the reply's PROGRESS lines.
    pub(crate) fn assistant_assessment(&self, params: &Value, prompt: &str) -> Result<()> {
        let _lock = self
            .command_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Its own request: the assessment is one row in the chat, apart from
        // whatever the user asks next.
        let request = format!("assess-{}", crate::id());
        crate::with_request(Some(&request), || {
            let generation = self.generation.load(Ordering::SeqCst);
            let turn = self.assistant_turn(params)?;
            self.event_for(
                Some(turn.id.as_str()),
                None,
                "working",
                "Assessing project progress",
                json!({"text":"Assess project progress"}),
            );
            let reply = match self.converse(prompt, &turn, generation) {
                Ok(reply) => reply,
                Err(error) => {
                    // Reported on the assessment's own row, which it closes.
                    self.event_for(
                        Some(turn.id.as_str()),
                        None,
                        "error",
                        &format!("Progress assessment failed: {error}"),
                        json!({"op":"assess"}),
                    );
                    return Ok(());
                }
            };
            if reply["stopped"] != true && reply["progress_recorded"].as_u64() == Some(0) {
                self.event_for(
                    Some(turn.id.as_str()),
                    None,
                    "warning",
                    "The assistant answered without PROGRESS lines, so no estimate was recorded",
                    json!({}),
                );
            }
            Ok(())
        })
    }

    /// Which project a command is about: one it names, else the project
    /// selected in the UI, else the only project.
    fn command_project(&self, text: &str, params: &Value) -> Result<Value> {
        let projects = self.projects()?;
        let spoken = conversational_text(text).to_lowercase();
        for marker in [" in ", " for ", " on ", " of "] {
            let Some((_, suffix)) = spoken.rsplit_once(marker) else {
                continue;
            };
            let target = suffix
                .split(" called ")
                .next()
                .unwrap_or_default()
                .split(" named ")
                .next()
                .unwrap_or_default()
                .trim()
                .trim_end_matches(['.', ',', '!', '?'])
                .trim_end_matches(" please")
                .trim();
            let target = target.strip_prefix("the ").unwrap_or(target);
            let target = target.strip_suffix(" project").unwrap_or(target).trim();
            if let Some(project) = projects.iter().find(|project| {
                project["name"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase() == target)
            }) {
                return Ok(project.clone());
            }
        }
        let named: Vec<&Value> = projects
            .iter()
            .filter(|project| mentioned(&spoken, project["name"].as_str().unwrap_or("")))
            .collect();
        if named.len() == 1 {
            return Ok(named[0].clone());
        }
        if let Some(reference) = params["project_id"].as_str().filter(|value| !value.trim().is_empty())
            && let Ok(project) = self.resolve_project(reference)
        {
            if params["source"] != "cli" {
                *self.current_project.lock().unwrap() = project["id"].as_str().map(str::to_string);
            }
            return Ok(project);
        }
        if let Some(project) = self.station_project(&projects) {
            return Ok(project);
        }
        match projects.len() {
            1 => Ok(projects[0].clone()),
            0 => Err("Add a project first".into()),
            _ => Err("Say which project, for example “in Navi”".into()),
        }
    }

    /// Deterministic commands that need no decision model: open a harness,
    /// switch to a session, ask for or set progress, run the detector. `None`
    /// leaves the clause to the model and the conversation bridge.
    fn fast_intent(&self, clause: &str) -> Result<Option<Value>> {
        let text = conversational_text(clause);
        let lower = text
            .to_lowercase()
            .trim()
            .trim_end_matches(['.', '!', '?'])
            .trim()
            .to_string();
        // Navi's own slash commands read as the sentence they stand for; any
        // other slash command belongs to the assistant.
        let lower = match lower.split_once(' ').map_or((lower.as_str(), ""), |(head, rest)| (head, rest)) {
            ("/progress", _) => "show the progress".to_string(),
            ("/help", _) => "help".to_string(),
            ("/sessions", _) => "what's running".to_string(),
            ("/assess", _) => "assess the project".to_string(),
            ("/open" | "/mark" | "/set" | "/reopen", rest) if !rest.trim().is_empty() => {
                lower.trim_start_matches('/').to_string()
            }
            _ => lower,
        };
        if matches!(lower.as_str(), "help" | "what can you do" | "what can i say" | "what can i ask"
            | "what can you do for me" | "what commands are there")
        {
            return Ok(Some(json!({"kind":"help"})));
        }
        // Only a wake word or filler ("Navi?", "okay"): ask what they need;
        // the question also opens the voice follow-up window.
        let only_leads = lower
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | ':' | '.' | '!' | '?'))
            .filter(|word| !word.is_empty())
            .all(|word| {
                matches!(word, "hey" | "hi" | "hello" | "navi" | "okay" | "ok" | "umm" | "um" | "uhh"
                    | "uh" | "erm" | "hmm" | "alright")
            });
        if only_leads && !lower.is_empty() {
            return Ok(Some(json!({"kind":"ack","reply":"Say “help” for a list. What do you need?"})));
        }
        // "Thanks, Navi", "never mind": answered here instead of waking the
        // assistant (or the slower model route) for a one-word courtesy.
        let bare = lower.strip_suffix(" navi").unwrap_or(&lower).trim_end_matches([',', ' ']);
        if matches!(bare, "thanks" | "thank you" | "thanks a lot" | "thank you so much" | "cheers"
            | "great thanks" | "perfect thanks" | "ok thanks" | "okay thanks" | "thx")
        {
            return Ok(Some(json!({"kind":"ack","reply":"You're welcome."})));
        }
        if matches!(bare, "never mind" | "nevermind" | "cancel" | "forget it" | "forget that"
            | "that's all" | "that is all" | "nothing" | "no thanks" | "no thank you")
        {
            return Ok(Some(json!({"kind":"ack","reply":"Okay."})));
        }
        // The clock, read locally: a voice question that needs no model.
        if matches!(bare, "what time is it" | "what's the time" | "whats the time" | "what is the time" | "time"
            | "what time is it now" | "do you have the time")
        {
            return Ok(Some(json!({"kind":"ack","reply":clock_sentence(false)})));
        }
        if matches!(bare, "what's the date" | "whats the date" | "what is the date" | "what's today's date"
            | "what day is it" | "what day is it today" | "what's the date today" | "what is today" | "date")
        {
            return Ok(Some(json!({"kind":"ack","reply":clock_sentence(true)})));
        }
        // Station dialogs: the reply carries the action the window takes.
        let dialog = match bare {
            "settings" | "open settings" | "open the settings" | "show settings" | "show the settings"
            | "open preferences" | "preferences" => Some(("show_settings", "Opened settings.")),
            "add a project" | "add project" | "add a new project" | "new project" | "register a project" => {
                Some(("add_project", "Choose the project's folder."))
            }
            "fullscreen" | "full screen" | "go fullscreen" | "go full screen" | "toggle fullscreen"
            | "toggle full screen" | "exit fullscreen" | "exit full screen" | "leave full screen"
            | "leave fullscreen" => Some(("fullscreen", "Toggled full screen.")),
            _ => None,
        };
        // "Stop listening", "mute": the microphone and spoken-reply settings.
        let toggle = match bare {
            "stop listening" | "turn off the microphone" | "turn off the mic" | "turn the microphone off"
            | "turn the mic off" | "mic off" | "microphone off" | "disable the microphone" | "mute the microphone"
            | "mute the mic" => Some(("microphone", false)),
            "start listening" | "turn on the microphone" | "turn on the mic" | "turn the microphone on"
            | "turn the mic on" | "mic on" | "microphone on" | "enable the microphone" | "unmute the microphone"
            | "unmute the mic" => Some(("microphone", true)),
            "mute" | "be quiet" | "quiet" | "shush" | "stop talking" | "stop speaking" | "don't speak"
            | "speech off" | "turn off speech" | "turn speech off" | "stop reading replies" | "mute replies" => {
                Some(("speak", false))
            }
            "unmute" | "speak" | "talk to me" | "speak replies" | "read replies aloud" | "read replies out loud"
            | "speech on" | "turn on speech" | "turn speech on" | "start talking" | "start speaking"
            | "unmute replies" => Some(("speak", true)),
            _ => None,
        };
        if matches!(bare, "undo" | "undo that" | "undo it" | "undo the last change" | "undo my last change"
            | "take that back" | "revert that" | "scratch that" | "that was wrong" | "that's wrong")
        {
            return Ok(Some(json!({"kind":"undo"})));
        }
        if let Some((key, on)) = toggle {
            return Ok(Some(json!({"kind":"setting","key":key,"on":on})));
        }
        if let Some((action, reply)) = dialog {
            return Ok(Some(json!({"kind":"ui","action":action,"reply":reply})));
        }
        if ["which project am i in", "what project am i in", "which project is this",
            "what project is this", "list projects", "list the projects", "list my projects",
            "what projects", "which projects", "my projects", "compare the projects", "compare projects",
            "all projects", "every project", "each project"]
            .iter()
            .any(|phrase| lower.contains(phrase))
        {
            return Ok(Some(json!({"kind":"project_list"})));
        }
        // "What did Codex report?": that harness's recorded estimates.
        let reported = ["what did ", "what has ", "what's ", "whats "]
            .iter()
            .find_map(|lead| lower.strip_prefix(lead))
            .and_then(|rest| {
                [" report", " reported", " estimate", " estimated", " record", " recorded", " report on the progress"]
                    .iter()
                    .find_map(|tail| rest.strip_suffix(tail))
            })
            .map(|name| name.strip_prefix("the ").unwrap_or(name).trim());
        if let Some(name) = reported {
            let settings = self.settings();
            let source = settings["profiles"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|profile| {
                    ["name", "id"].iter().any(|key| profile[*key].as_str().is_some_and(|value| value.eq_ignore_ascii_case(name)))
                        || (profile["id"] == "claude" && name == "claude code")
                })
                .and_then(|profile| profile["id"].as_str().map(str::to_string))
                .or_else(|| matches!(name, "detector" | "the detector" | "git").then(|| name.to_string()));
            if let Some(source) = source {
                return Ok(Some(json!({"kind":"reports","source":source})));
            }
        }
        if let Some(screen) = self.screen_intent(&lower)? {
            return Ok(Some(screen));
        }
        // "Tell Codex to run the tests": typed into that session, as voice does.
        if let Some(prompt) = self.harness_prompt_intent(&text)? {
            return Ok(Some(prompt));
        }
        // Before the negation guard, which reads any "stop" as one: this only
        // takes a verb followed by exactly one session's name.
        if let Some(control) = self.session_control_intent(&lower)? {
            return Ok(Some(control));
        }
        // "Add a task called fix login to the daemon", "add a part called docs".
        if let Some((title, split)) = parse_task_add(&text) {
            return Ok(Some(json!({"kind":"task_add","title":title,"split":split})));
        }
        if let Some(title) = parse_part_add(&text) {
            return Ok(Some(json!({"kind":"part_add","title":title})));
        }
        if lower.is_empty() || negated(&lower) {
            return Ok(None);
        }
        let short = lower.split_whitespace().count() <= 14;
        // "Close the progress", "hide the graph": the panel is window state.
        if short
            && ["close", "hide", "dismiss"].iter().any(|word| mentioned(&lower, word))
            && ["progress", "graph", "panel"].iter().any(|word| mentioned(&lower, word))
        {
            return Ok(Some(json!({"kind":"progress_hide"})));
        }
        if short && let Some((part, percent)) = parse_set_progress(&lower) {
            return Ok(Some(json!({"kind":"progress_set","part":part,"percent":percent})));
        }
        if short && let Some((task, state)) = parse_task_state(&lower) {
            return Ok(Some(json!({"kind":"task_state","task":task,"state":state})));
        }
        if short
            && let Some((task, state)) = parse_task_statement(&lower)
            && self.names_task_or_part(clause, &task)
        {
            return Ok(Some(json!({"kind":"task_state","task":task,"state":state})));
        }
        let asking = [
            "how far along", "how far are we", "how far is", "how much is done", "how much of",
            "how complete", "how close", "how much is left", "how much left", "what percent",
            "completion percentage", "completion percent", "percent complete", "percent done",
            "what percentage", "progress report", "what's the progress", "what is the progress",
            "whats the progress", "project status", "status of the project", "show progress",
            "show the progress", "show me the progress", "open the progress", "open progress",
            "check progress", "check the progress", "how is the project", "how's the project",
            "how are we doing", "what's left", "what is left", "whats left", "what remains",
            "what's remaining", "what is remaining", "how much remains", "show the graph",
            "show me the graph", "open the graph", "progress graph", "progress panel",
            "is the project done", "is the project finished", "is the project complete",
            "are we done", "are we finished", "are we there yet", "what's next", "what is next",
            "whats next", "what should i work on", "what should i do next", "what do i do next",
            "needs doing", "need doing", "left to do", "still to do", "where are we",
            "status update", "coming along", "how's it going", "how is it going", "bring up the progress",
            "pull up the progress", "bring up the graph", "pull up the graph", "can i see the progress",
            "like to see the progress", "what's the percentage", "what is the percentage",
            "whats the percentage", "furthest behind", "farthest behind", "weakest part", "least done",
            "most behind", "lagging behind",
        ]
        .iter()
        .chain(BLOCKED_PHRASES.iter())
        .chain(DONE_PHRASES.iter())
        .chain(TOPIC_PHRASES.iter().flat_map(|(_, phrases)| phrases.iter()))
        .chain(RECENT_PHRASES.iter())
        .chain(STALE_PHRASES.iter())
        .any(|phrase| lower.contains(phrase))
            // "Is the voice part done?" asks about one part.
            || (lower.starts_with("is the ")
                && mentioned(&lower, "part")
                && ["done", "finished", "complete"].iter().any(|word| lower.ends_with(word)))
            // "What's the status" of the project, not "… of Codex".
            || (["what's the status", "what is the status", "whats the status"]
                .iter()
                .any(|phrase| lower.starts_with(phrase))
                && lower.split_once(" status of ").is_none_or(|(_, rest)| {
                    let rest = rest.strip_prefix("the ").unwrap_or(rest);
                    matches!(rest, "project" | "this project" | "repo" | "codebase" | "progress")
                        || self.projects().is_ok_and(|projects| {
                            projects.iter().any(|project| {
                                project["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(rest))
                            })
                        })
                }))
            // "How is the daemon doing?", "what's voice at?", "show me the
            // voice part": one part or task by name.
            || ["how is ", "how's ", "hows ", "what's ", "what is ", "whats ", "where's ", "where is ",
                "show me ", "show ", "open "]
                .iter()
                .find_map(|lead| lower.strip_prefix(lead))
                .and_then(|rest| {
                    [" doing", " going", " coming along", " looking", " at", " part"]
                        .iter()
                        .find_map(|tail| rest.strip_suffix(tail))
                        .or(lower.starts_with("how").then_some(rest))
                })
                .map(|name| name.strip_prefix("the ").unwrap_or(name).trim().to_string())
                .is_some_and(|name| {
                    !name.is_empty() && name.split_whitespace().count() <= 5 && self.names_part(clause, &name)
                })
            // "Is barge-in done?" asks about one task or part by name.
            || (lower.strip_prefix("is ").and_then(|rest| {
                let rest = rest.strip_suffix(" yet").unwrap_or(rest);
                ["done", "finished", "complete", "blocked"]
                    .iter()
                    .find_map(|word| rest.strip_suffix(word))
                    .map(|name| name.trim().to_string())
            }))
            .is_some_and(|name| !name.is_empty() && self.names_task_or_part(clause, &name));
        let core = [" for ", " of ", " in ", " on "]
            .iter()
            .filter_map(|marker| lower.find(marker))
            .min()
            .map_or(lower.as_str(), |cut| &lower[..cut]);
        let core = core.strip_suffix(" please").unwrap_or(core);
        let last = core.split_whitespace().last().unwrap_or_default();
        let strong = ["progress", "completion", "percent", "percentage", "detector"]
            .iter()
            .any(|word| mentioned(&lower, word))
            && progress_subject(&lower);
        // A bare "reassess" means the active project.
        let whole = matches!(last, "project" | "repo" | "repository" | "codebase" | "progress"
            | "completion" | "detector" | "percentage" | "estimate" | "estimates")
            || core.split_whitespace().count() == 1
            || (!last.is_empty()
                && self.projects()?.iter().any(|project| {
                    project["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(last))
                }));
        let assess = ["detect", "assess", "reassess", "re-assess", "evaluate", "analyze",
            "analyse", "scan", "rescan", "re-scan", "redetect", "re-detect", "estimate",
            "re-estimate", "reestimate"]
            .iter()
            .any(|word| mentioned(&lower, word));
        let refresh = ["update", "refresh", "recheck", "re-check", "recompute", "measure", "run"]
            .iter()
            .any(|word| mentioned(&lower, word));
        // "When was the last estimate?" asks; it must not start an assessment.
        let question = ["when ", "how ", "what", "why ", "which ", "who ", "where ", "is ", "are ", "was ",
            "did ", "does ", "has "]
            .iter()
            .any(|word| lower.starts_with(word));
        if short
            && !asking
            && !question
            && ((assess && whole) || (refresh && strong && whole) || lower.contains("progress check"))
        {
            return Ok(Some(json!({"kind":"detect"})));
        }
        if short && asking && progress_subject(&lower) {
            let show = ["show", "open", "display", "bring up", "pull up"].iter().any(|word| mentioned(&lower, word));
            let left = ["what's left", "what is left", "whats left", "what remains", "what's remaining",
                "what is remaining", "how much is left", "how much left", "how much remains",
                "what's next", "what is next", "whats next", "what should i work on",
                "what should i do next", "what do i do next", "needs doing", "need doing",
                "left to do", "still to do"]
                .iter()
                .any(|phrase| lower.contains(phrase));
            let recent = RECENT_PHRASES.iter().any(|phrase| lower.contains(phrase));
            let stale = STALE_PHRASES.iter().any(|phrase| lower.contains(phrase));
            let blocked = BLOCKED_PHRASES.iter().any(|phrase| lower.contains(phrase));
            let done = DONE_PHRASES.iter().any(|phrase| lower.contains(phrase));
            let topic = TOPIC_PHRASES
                .iter()
                .find(|(_, phrases)| phrases.iter().any(|phrase| lower.contains(phrase)))
                .map(|(topic, _)| *topic);
            return Ok(Some(json!({"kind":"progress","show":show,"left":left,"recent":recent,
                "stale":stale,"blocked":blocked,"done":done,"topic":topic})));
        }
        if short && let Some(status) = self.session_status_intent(&lower)? {
            return Ok(Some(status));
        }
        // "Switch projects", "the other project": the next project in the list.
        if matches!(bare, "switch projects" | "switch project" | "change project" | "change projects"
            | "next project" | "the other project" | "other project" | "go to the other project"
            | "switch to the other project" | "previous project" | "the next project" | "go to the next project")
        {
            return Ok(Some(json!({"kind":"project_cycle","back":bare.contains("previous")})));
        }
        // "I need a terminal", "give me another Codex": an open request.
        let lower = ["i need another ", "give me another ", "get me another "]
            .iter()
            .find_map(|prefix| lower.strip_prefix(prefix).map(|rest| format!("open a new {rest}")))
            .or_else(|| {
                ["i need a ", "i need an ", "give me a ", "give me an ", "get me a ", "get me an ", "i want a ",
                    "i want an "]
                    .iter()
                    .find_map(|prefix| lower.strip_prefix(prefix).map(|rest| format!("open a {rest}")))
            })
            .unwrap_or(lower);
        if !immediate_request(&lower) {
            return Ok(None);
        }
        let settings = self.settings();
        let switching = ["switch", "go back", "go to", "return to", "select", "focus", "jump to",
            "change to", "move to", "take me to", "show me", "show", "pull up"]
            .iter()
            .any(|word| mentioned(&lower, word));
        let resuming = ["resume", "continue", "existing"]
            .iter()
            .any(|word| mentioned(&lower, word));
        let launching = ["open", "launch", "start", "create", "new", "fresh", "bring up",
            "fire up", "spin up", "boot up"]
            .iter()
            .any(|word| mentioned(&lower, word));
        if switching && !resuming {
            let sessions = self.sessions()?;
            let matched: Vec<&Value> = sessions
                .iter()
                .filter(|session| {
                    session["running"] == true
                        && mentioned(&lower, session["name"].as_str().unwrap_or(""))
                })
                .collect();
            if matched.len() == 1 {
                return Ok(Some(json!({"kind":"switch","id":matched[0]["id"],
                    "name":matched[0]["name"]})));
            }
            // Several by that name ("Codex" here and in another project):
            // the run step prefers the one in the command's project.
            if matched.len() > 1 {
                let ids: Vec<Value> = matched.iter().map(|session| session["id"].clone()).collect();
                return Ok(Some(json!({"kind":"switch","ids":ids,"name":matched[0]["name"]})));
            }
            // No session by that name: "switch to acme-api" names a project.
            let projects = self.projects()?;
            let named: Vec<&Value> = projects
                .iter()
                .filter(|project| mentioned(&lower, project["name"].as_str().unwrap_or("")))
                .collect();
            if named.len() == 1 {
                return Ok(Some(json!({"kind":"project_switch","id":named[0]["id"],
                    "name":named[0]["name"]})));
            }
            return Ok(None);
        }
        if !launching || resuming {
            return Ok(None);
        }
        let profiles = settings["profiles"].as_array().cloned().unwrap_or_default();
        let named: Vec<&Value> = profiles
            .iter()
            .filter(|profile| {
                mentioned(&lower, profile["name"].as_str().unwrap_or(""))
                    || mentioned(&lower, profile["id"].as_str().unwrap_or(""))
                    // "open codex" contains the letters "open code".
                    || (profile["id"] == "opencode" && mentioned(&lower, "open code"))
            })
            .collect();
        let specific: Vec<&Value> = named
            .iter()
            .copied()
            .filter(|profile| profile["id"] != "terminal")
            .collect();
        let chosen = match (specific.len(), named.len()) {
            (1, _) => specific[0],
            (0, 1) => named[0],
            (0, 0) => {
                // "Open the acme-api project" names a project, not a harness.
                let projects = self.projects()?;
                let named: Vec<&Value> = projects
                    .iter()
                    .filter(|project| mentioned(&lower, project["name"].as_str().unwrap_or("")))
                    .collect();
                if named.len() == 1 {
                    return Ok(Some(json!({"kind":"project_switch","id":named[0]["id"],
                        "name":named[0]["name"]})));
                }
                return Ok(None);
            }
            _ => return Ok(None),
        };
        Ok(Some(json!({"kind":"launch","profile":chosen["id"],"name":chosen["name"]})))
    }

    /// "Tell Codex to run the tests", "ask Claude what it changed", "continue
    /// in Codex: add a test": the same routes the voice worker takes. The
    /// target must name an open session ("this" is the active one), so "tell
    /// me a joke" is left alone. The prompt keeps the user's casing.
    fn harness_prompt_intent(&self, text: &str) -> Result<Option<Value>> {
        let trimmed = text.trim();
        // ASCII lowercasing keeps byte offsets equal to the original's.
        let lower = trimmed.to_ascii_lowercase();
        // "Say hello to Codex", "type ls in the terminal": the prompt comes
        // first and the session last.
        for (verb, marker) in [("say ", " to "), ("send ", " to "), ("type ", " in "), ("type ", " into ")] {
            let Some(body) = trimmed.get(verb.len()..).filter(|_| lower.starts_with(verb)) else {
                continue;
            };
            let Some(at) = body.to_ascii_lowercase().rfind(marker) else {
                continue;
            };
            let prompt = body[..at].trim().trim_matches(['"', '“', '”']).trim();
            let target = body[at + marker.len()..].trim().trim_end_matches(['.', '!', '?']);
            if prompt.is_empty() || target.is_empty() {
                continue;
            }
            let reordered = format!("tell {target} to {prompt}");
            // Only an open session: "send the error log to Claude" must not
            // start Claude with "the error log" as its prompt.
            if let Some(action) = self.harness_prompt_intent(&reordered)?.filter(|action| action["kind"] == "harness_prompt") {
                return Ok(Some(action));
            }
        }
        let rest_at = ["tell ", "ask ", "send ", "message ", "continue in ", "have ", "get ", "give "]
            .iter()
            .find_map(|verb| lower.starts_with(verb).then_some(verb.len()));
        // No verb: "Codex #1, run the tests" and "terminal: ls" address an
        // open session directly; the name must end at the comma or colon.
        let direct = rest_at.is_none();
        if direct && !lower.contains([':', ',']) {
            return Ok(None);
        }
        let start = rest_at.unwrap_or(0);
        let rest = &trimmed[start..];
        let rest_lower = &lower[start..];
        let sessions = self.sessions()?;
        let labels = session_labels(&sessions);
        let active = self.pronoun_session();
        let find = |name: &str| named_sessions(&sessions, &labels, active.as_deref(), name);
        // "<target> to <text>" first (multi-word names), then a one-word
        // target followed by the text.
        let mut candidates: Vec<(usize, usize)> = if direct {
            Vec::new()
        } else {
            rest_lower
                .match_indices(" to ")
                .map(|(index, marker)| (index, index + marker.len()))
                .collect()
        };
        if direct {
            candidates.extend(rest_lower.find([':', ',']).map(|mark| (mark, mark + 1)));
        } else {
            // No "to": the longest leading name that is a session ("tell
            // Codex #1 what's left" is Codex #1, not Codex).
            let mut splits: Vec<(usize, usize)> = rest_lower
                .match_indices([' ', ':', ','])
                .map(|(index, _)| (index, index + 1))
                .collect();
            splits.reverse();
            candidates.extend(splits);
        }
        for &(end, text_start) in &candidates {
            let ids = find(&rest[..end]);
            let prompt = rest[text_start..].trim().trim_start_matches([':', ',']).trim();
            // "Give Codex the next task" hands over recorded work, nothing else.
            let given = !lower.starts_with("give ") || work_request(prompt).is_some();
            if !ids.is_empty() && !prompt.is_empty() && given {
                return Ok(Some(json!({"kind":"harness_prompt","ids":ids,"text":prompt})));
            }
        }
        // No open session by that name, but it names a harness ("ask Claude
        // to fix the build" with Claude closed): open it with the prompt.
        if direct {
            return Ok(None);
        }
        let settings = self.settings();
        let profiles = settings["profiles"].as_array().cloned().unwrap_or_default();
        for &(end, text_start) in &candidates {
            let name = rest_lower[..end].trim().trim_end_matches([':', ',']);
            let name = name.strip_prefix("the ").unwrap_or(name);
            let prompt = rest[text_start..].trim().trim_start_matches([':', ',']).trim();
            let profile = profiles.iter().find(|profile| {
                profile["id"] != "terminal"
                    && (profile["id"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(name))
                        || profile["name"].as_str().is_some_and(|label| label.eq_ignore_ascii_case(name))
                        || (profile["id"] == "claude" && name == "claude code"))
            });
            let given = !lower.starts_with("give ") || work_request(prompt).is_some();
            if let Some(profile) = profile.filter(|_| !prompt.is_empty() && given) {
                return Ok(Some(json!({"kind":"launch","profile":profile["id"],
                    "name":profile["name"],"prompt":prompt})));
            }
        }
        Ok(None)
    }

    /// "What did Codex say?", "what's on the terminal's screen", "read me the
    /// last output": the last lines on that session's screen ("it" and a
    /// bare question mean the active session).
    fn screen_intent(&self, lower: &str) -> Result<Option<Value>> {
        let mut name = None;
        for lead in ["what did ", "what does ", "what is ", "what's ", "whats "] {
            let Some(rest) = lower.strip_prefix(lead) else {
                continue;
            };
            name = [" say last", " say", " saying", " print", " printing", " output", " show", " showing",
                " reply", " answer", " write", " asking me", " asking for", " asking"]
                .iter()
                .find_map(|tail| rest.strip_suffix(tail));
            if name.is_some() {
                break;
            }
        }
        for lead in ["what's on ", "what is on ", "whats on ", "read me ", "read ", "show me ", "show "] {
            let Some(rest) = lower.strip_prefix(lead).filter(|_| name.is_none()) else {
                continue;
            };
            let on = lead.contains(" on ");
            name = [" screen", " output", " last output", " latest output"]
                .iter()
                .find_map(|tail| rest.strip_suffix(tail))
                .or_else(|| on.then_some(rest).filter(|rest| !rest.contains(' ') || rest.starts_with("the ")));
        }
        let Some(name) = name else {
            return Ok(None);
        };
        let mut name = name.trim();
        name = name.strip_suffix("'s").unwrap_or(name);
        name = name.strip_prefix("the ").unwrap_or(name);
        for suffix in [" last", " latest", " session", " pane"] {
            name = name.strip_suffix(suffix).unwrap_or(name);
        }
        let name = match name {
            "" | "the" | "last" | "latest" | "that" | "screen" => "it",
            name => name,
        };
        let sessions = self.sessions()?;
        let labels = session_labels(&sessions);
        let active = self.pronoun_session();
        let ids = named_sessions(&sessions, &labels, active.as_deref(), name);
        if !ids.is_empty() {
            return Ok(Some(json!({"kind":"session_screen","ids":ids})));
        }
        // "What did Pi say" with Pi closed.
        let settings = self.settings();
        let absent = settings["profiles"].as_array().into_iter().flatten().find(|profile| {
            ["name", "id"].iter().any(|key| profile[*key].as_str().is_some_and(|value| value.eq_ignore_ascii_case(name)))
                || (profile["id"] == "claude" && name == "claude code")
        });
        Ok(absent.map(|profile| json!({"kind":"session_status","ids":[],"waiting":false,"absent":profile["name"]})))
    }

    /// "Stop Codex", "close terminal 2": interrupt or close one open session.
    /// Sessions are named as the strip names them, so "#2" (or "two") picks
    /// one of several same-named panes.
    fn session_control_intent(&self, lower: &str) -> Result<Option<Value>> {
        let mut command = lower;
        for prefix in ["please ", "can you ", "could you ", "would you "] {
            command = command.strip_prefix(prefix).unwrap_or(command);
        }
        let (verb, object) = command.split_once(' ').unwrap_or((command, ""));
        let op = match verb {
            "stop" | "interrupt" | "pause" => "interrupt",
            "close" | "quit" | "exit" | "kill" | "end" | "terminate" => "kill",
            "reopen" | "resume" => "reopen",
            "restart" | "reboot" | "relaunch" => "restart",
            _ => return Ok(None),
        };
        // The object must be the session and nothing more, so "stop opening
        // Codex" or "close the progress" are not read as session commands.
        let mut object = object.trim();
        object = object.strip_prefix("the ").unwrap_or(object);
        for suffix in [" please", " now", " session", " pane", " tab"] {
            object = object.strip_suffix(suffix).unwrap_or(object);
        }
        let object = spoken_numbers(object);
        if object.is_empty() {
            return Ok(None);
        }
        // "Stop everything", "close all terminals": every open session here,
        // or those of one kind.
        if matches!(op, "interrupt" | "kill") {
            let every = match object.as_str() {
                "all" | "everything" | "all of them" | "every session" | "all sessions" | "all the sessions"
                | "all harnesses" | "all the harnesses" | "all my sessions" => Some(String::new()),
                other => other
                    .strip_prefix("all the ")
                    .or_else(|| other.strip_prefix("all my "))
                    .or_else(|| other.strip_prefix("all "))
                    .or_else(|| other.strip_prefix("every "))
                    .map(|kind| kind.trim_end_matches(" sessions").trim_end_matches(" session").to_string()),
            };
            if let Some(kind) = every {
                return Ok(Some(json!({"kind":"session_control_all","op":op,"filter":kind})));
            }
        }
        let sessions = self.sessions()?;
        if op == "reopen" {
            // A finished session, by its Recent label ("Codex #2") or name,
            // newest first. Nothing finished matches: not a session command,
            // so "reopen barge-in" still reaches the task intent.
            let finished = finished_labels(&sessions);
            let named = |session: &&Value| {
                finished
                    .get(session["id"].as_str().unwrap_or_default())
                    .is_some_and(|label| spoken_numbers(label) == object)
                    || names_session(session, &object)
            };
            let mut matches: Vec<&Value> = sessions
                .iter()
                .filter(|session| finished.contains_key(session["id"].as_str().unwrap_or_default()))
                .filter(named)
                .collect();
            matches.sort_by_key(|session| std::cmp::Reverse(session["last_active"].as_u64().unwrap_or(0)));
            let ids: Vec<Value> = matches.iter().map(|session| session["id"].clone()).collect();
            if !ids.is_empty() {
                return Ok(Some(json!({"kind":"session_reopen","ids":ids})));
            }
            // "Reopen Codex" while Codex runs: say so rather than look for a task.
            let labels = session_labels(&sessions);
            let open = sessions.iter().filter(|session| session["running"] == true).find_map(|session| {
                let label = labels.get(session["id"].as_str().unwrap_or_default())?;
                if spoken_numbers(label) == object {
                    return Some(label.clone());
                }
                names_session(session, &object)
                    .then(|| session["name"].as_str().unwrap_or("That session").to_string())
            });
            return Ok(open.map(|label| json!({"kind":"session_reopen","ids":[],"open":label})));
        }
        // "Close Codex in acme-api": the form a "Which one" reply offers.
        let projects = self.projects()?;
        let (object, within) = object
            .rsplit_once(" in ")
            .and_then(|(head, tail)| {
                let project = projects.iter().find(|project| {
                    project["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(tail.trim()))
                })?;
                Some((head.trim().to_string(), Some(project["id"].clone())))
            })
            .unwrap_or((object, None));
        let labels = session_labels(&sessions);
        let running = || {
            sessions.iter().filter(|session| {
                session["running"] == true && within.as_ref().is_none_or(|id| &session["project_id"] == id)
            })
        };
        let mut ids: Vec<Value> = running()
            .filter(|session| {
                labels
                    .get(session["id"].as_str().unwrap_or_default())
                    .is_some_and(|label| spoken_numbers(label) == object)
            })
            .map(|session| session["id"].clone())
            .collect();
        if ids.is_empty() {
            ids = running()
                .filter(|session| names_session(session, &object))
                .map(|session| session["id"].clone())
                .collect();
        }
        Ok((!ids.is_empty()).then(|| json!({"kind":"session_control","op":op,"ids":ids})))
    }

    /// "Is Codex done?", "what is Claude doing", "what's running", "does
    /// anything need my input": a question about open harness sessions, read
    /// from their pane state instead of a model.
    fn session_status_intent(&self, lower: &str) -> Result<Option<Value>> {
        let waiting = ["need my input", "needs my input", "need input", "needs input",
            "waiting for me", "waiting on me", "need me", "needs me", "need my attention", "needs my attention",
            "need attention", "needs attention", "waiting for input", "waiting for an answer"]
            .iter()
            .any(|phrase| lower.contains(phrase));
        let overview = ["anything busy", "anything running", "anyone busy", "what's busy",
            "what's running", "what is running", "whats running", "what's open",
            "what is open", "which sessions", "which harnesses", "sessions doing",
            "harnesses doing", "status of the sessions", "status of the harnesses",
            "session status", "harness status", "list sessions", "list the sessions", "list my sessions",
            "list harnesses", "list the harnesses", "how many sessions", "how many harnesses",
            "how many terminals", "open sessions", "running sessions"]
            .iter()
            .any(|phrase| lower.contains(phrase));
        let yes_no = ["is ", "are ", "has ", "did "].iter().any(|prefix| lower.starts_with(prefix))
            && ["done", "finished", "finish", "complete", "ready", "waiting", "working", "busy", "still", "stuck", "open",
                "running", "idle", "alive", "blocked", "asking", "thinking"]
                .iter()
                .any(|word| mentioned(lower, word));
        let asking = yes_no
            || ["doing", "status of", "state of", "check on", "up to"]
                .iter()
                .any(|phrase| mentioned(lower, phrase));
        if !(waiting || overview || asking) {
            return Ok(None);
        }
        let sessions = self.sessions()?;
        let ids: Vec<Value> = sessions
            .iter()
            .filter(|session| session["running"] == true)
            .filter(|session| {
                ["name", "profile", "harness"].iter().any(|key| {
                    mentioned(lower, session[*key].as_str().unwrap_or(""))
                }) || (session["profile"] == "claude" && mentioned(lower, "claude code"))
            })
            .map(|session| session["id"].clone())
            .collect();
        if ids.is_empty() && !(waiting || overview) {
            // "Is Codex open?" with no Codex: say so rather than ask a model.
            let settings = self.settings();
            let absent = settings["profiles"].as_array().into_iter().flatten().find(|profile| {
                ["name", "id"].iter().any(|key| mentioned(lower, profile[*key].as_str().unwrap_or("")))
                    || (profile["id"] == "claude" && mentioned(lower, "claude code"))
            });
            return Ok(absent.map(|profile| {
                json!({"kind":"session_status","ids":[],"waiting":false,"absent":profile["name"]})
            }));
        }
        Ok(if !ids.is_empty() {
            Some(json!({"kind":"session_status","ids":ids,"waiting":waiting}))
        } else {
            Some(json!({"kind":"session_status","all":true,"waiting":waiting}))
        })
    }

    /// Run one fast intent and build its reply. `run_command` posts the reply
    /// of the whole command to the chat once every step has run.
    fn run_fast(&self, clause: &str, action: &Value, params: &Value) -> Result<Value> {
        let project = self.command_project(clause, params);
        // The reply reaches the chat once, from `run_command`, after every
        // step of the command has run.
        let reply = |project_id: Option<&str>, text: String, extra: Value| -> Value {
            let (persona, _, _) = self.assistant_target();
            let mut reply = json!({"text":text,"persona":persona,"source":"navi","project_id":project_id});
            if let (Some(target), Some(extra)) = (reply.as_object_mut(), extra.as_object()) {
                for (key, value) in extra {
                    target.insert(key.clone(), value.clone());
                }
            }
            reply
        };
        match action["kind"].as_str().unwrap_or_default() {
            "progress" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let graph = self.graph_get(Some(&id))?;
                // "How far is the voice part" asks about one part.
                let spoken = loose_title(clause);
                let named: Vec<&Value> = graph["aspects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|aspect| mentioned(&spoken, &loose_title(aspect["title"].as_str().unwrap_or(""))))
                    .collect();
                // "Is barge-in done?": one task named and no part.
                let tasks: Vec<(&Value, &Value)> = graph["aspects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|aspect| aspect["items"].as_array().into_iter().flatten().map(move |task| (task, aspect)))
                    .filter(|(task, _)| mentioned(&spoken, &loose_title(task["title"].as_str().unwrap_or(""))))
                    .collect();
                let mut text = match (named.as_slice(), tasks.as_slice()) {
                    ([], [(task, aspect)]) => {
                        let state = match task["state"].as_str().unwrap_or_default() {
                            "done" if task["verified"] == true => "done and verified",
                            "done" => "done",
                            "in_progress" => "in progress",
                            "blocked" => "blocked",
                            _ => "not started",
                        };
                        format!(
                            "“{}” in {} is {state}.",
                            task["title"].as_str().unwrap_or("That task"),
                            aspect["title"].as_str().unwrap_or("its part")
                        )
                    }
                    _ => match named.as_slice() {
                    [aspect] => store::part_sentence(&graph, aspect),
                    _ if action["recent"] == true => {
                        store::recent_sentence(&graph, crate::now().saturating_sub(86_400_000))
                    }
                    _ if action["topic"] == "estimate" => estimate_sentence(&graph),
                    _ if action["topic"] == "parts" => parts_sentence(&graph),
                    _ if action["topic"] == "active" => active_sentence(&graph),
                    _ if action["topic"] == "next" => next_task(&graph).0,
                    _ if action["topic"] == "count" => count_sentence(&graph),
                    _ if action["topic"] == "tasks" => store::remaining_sentence(&graph),
                    _ if action["blocked"] == true => store::blocked_sentence(&graph),
                    _ if action["done"] == true => store::done_sentence(&graph),
                    _ if action["left"] == true => store::remaining_sentence(&graph),
                    _ => store::progress_sentence(&graph),
                    },
                };
                if graph["percent"].is_null() {
                    text.push_str(" Say “assess the project” and I will ask a harness to judge it.");
                } else if action["stale"] == true {
                    // The headline sentence already names a nonzero count.
                    match graph["commits_since"].as_i64().unwrap_or(0) {
                        0 => text.push_str(" No commits landed since the last estimate."),
                        _ => text.push_str(" Say “reassess” to update it."),
                    }
                }
                let extra = json!({"action": if action["show"] == true { "show_graph" } else { "" },
                    "percent": graph["percent"]});
                Ok(json!({"reply": reply(Some(&id), text, extra)}))
            }
            "detect" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let result = self.project_detect(&json!({"project_id": id}))?;
                let text = result["message"].as_str().unwrap_or("Assessment started.").to_string();
                Ok(json!({"detect": {"via": result["via"], "started": result["started"]},
                    "reply": reply(Some(&id), text, json!({"action":"show_graph"}))}))
            }
            "progress_set" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let evidence: String = clause.trim().chars().take(200).collect();
                let part = action["part"].as_str().filter(|part| {
                    !project["name"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(part))
                });
                let before = self.last_signal_id()?;
                let result = self.progress_report(&json!({
                    "project_id": id,
                    "aspect": part,
                    "percent": action["percent"],
                    "evidence": format!("user said: {evidence}"),
                    "confidence": 1.0,
                    "source": "user",
                    "create": false,
                }));
                let text = match result {
                    Ok(result) => {
                        let target = result["aspect"].as_str().unwrap_or("The project");
                        self.remember_change(&id, before, None, &format!(
                            "{target} at {}%",
                            action["percent"].as_f64().unwrap_or(0.0).round()
                        ))?;
                        format!(
                            "{target} is now at {}%. {}",
                            action["percent"].as_f64().unwrap_or(0.0).round(),
                            result["summary"].as_str().unwrap_or("")
                        )
                    }
                    Err(error) => {
                        let graph = self.graph_get(Some(&id))?;
                        let parts: Vec<&str> = graph["aspects"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|aspect| aspect["title"].as_str())
                            .collect();
                        if parts.is_empty() {
                            format!("{error}. This project has no parts yet.")
                        } else {
                            format!("{error}. Parts: {}.", parts.join(", "))
                        }
                    }
                };
                Ok(json!({"reply": reply(Some(&id), text, json!({"action":"show_graph"}))}))
            }
            "task_state" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let graph = self.graph_get(Some(&id))?;
                let tasks: Vec<&Value> = graph["aspects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|aspect| aspect["items"].as_array().into_iter().flatten())
                    .chain(graph["unassigned"].as_array().into_iter().flatten())
                    .collect();
                let spoken = action["task"].as_str().unwrap_or_default();
                let wanted = loose_title(spoken);
                // Speech rarely reproduces a title exactly ("barge in" for
                // "Barge-in"): an exact loose match wins, else one task whose
                // title contains the words (or is contained in them).
                let exact: Vec<&Value> = tasks
                    .iter()
                    .copied()
                    .filter(|task| loose_title(task["title"].as_str().unwrap_or("")) == wanted)
                    .collect();
                let partial: Vec<&Value> = tasks
                    .iter()
                    .copied()
                    .filter(|task| {
                        let title = loose_title(task["title"].as_str().unwrap_or(""));
                        !title.is_empty() && (title.contains(&wanted) || wanted.contains(&title))
                    })
                    .collect();
                let chosen = match (exact.len(), partial.len()) {
                    (1, _) => Some(exact[0]),
                    (0, 1) => Some(partial[0]),
                    _ => None,
                };
                let state = action["state"].as_str().unwrap_or("done");
                let text = match chosen {
                    Some(task) => {
                        let title = task["title"].as_str().unwrap_or(spoken);
                        let evidence: String = clause.trim().chars().take(200).collect();
                        let before = self.last_signal_id()?;
                        let said = match state {
                            "in_progress" => "in progress",
                            "todo" => "to do",
                            other => other,
                        };
                        let restore = task["id"].as_str().zip(task["state"].as_str());
                        if state == "dropped" {
                            // Dropped tasks leave the graph and its percent.
                            self.set_task(&json!({"id": task["id"], "state": "dropped"}))?;
                            self.remember_change(&id, before, restore, &format!("dropping “{title}”"))?;
                            self.event_for(Some(&id), None, "progress", &format!("Dropped “{title}”"), json!({"task": task["id"], "source": "user"}));
                            let graph = self.graph_get(Some(&id))?;
                            format!("Dropped “{title}”; it no longer counts. {}", store::progress_sentence(&graph))
                        } else {
                        match self.progress_report(&json!({
                            "project_id": id,
                            "task_id": task["id"],
                            "state": state,
                            "evidence": format!("user said: {evidence}"),
                            "confidence": 1.0,
                            "source": "user",
                            "create": false,
                        })) {
                            Ok(result) => {
                                self.remember_change(&id, before, restore, &format!("“{title}” marked {said}"))?;
                                format!("Marked “{title}” {said}. {}", result["summary"].as_str().unwrap_or(""))
                            }
                            Err(error) => error,
                        }
                        }
                    }
                    // "Mark the detector done" names a part, not a task: the
                    // part is finished.
                    None if state == "done" && exact.is_empty() && partial.is_empty() && graph["aspects"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|aspect| loose_title(aspect["title"].as_str().unwrap_or("")) == wanted) =>
                    {
                        let part = graph["aspects"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|aspect| loose_title(aspect["title"].as_str().unwrap_or("")) == wanted)
                            .cloned()
                            .unwrap_or_default();
                        let evidence: String = clause.trim().chars().take(200).collect();
                        let before = self.last_signal_id()?;
                        match self.progress_report(&json!({
                            "project_id": id,
                            "aspect_id": part["id"],
                            "percent": 100.0,
                            "evidence": format!("user said: {evidence}"),
                            "confidence": 1.0,
                            "source": "user",
                            "create": false,
                        })) {
                            Ok(result) => {
                                let title = part["title"].as_str().unwrap_or(spoken);
                                self.remember_change(&id, before, None, &format!("{title} at 100%"))?;
                                format!("{title} is now at 100%. {}", result["summary"].as_str().unwrap_or(""))
                            }
                            Err(error) => error,
                        }
                    }
                    None => {
                        let candidates = if exact.len() + partial.len() > 1 {
                            exact.iter().chain(partial.iter()).copied().collect::<Vec<_>>()
                        } else {
                            tasks.clone()
                        };
                        let names: Vec<&str> = candidates
                            .iter()
                            .filter_map(|task| task["title"].as_str())
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .take(8)
                            .collect();
                        if tasks.is_empty() {
                            "This project has no tasks yet. A harness adds them as it reports, or add one in the progress panel.".to_string()
                        } else if exact.len() + partial.len() > 1 {
                            // "The second one" or a title answers it, as for sessions.
                            let choices: Vec<Value> =
                                names.iter().map(|title| json!({"label":title,"task":title})).collect();
                            *self.pending_choice.lock().unwrap() =
                                Some(json!({"time":now(),"action":action,"choices":choices}));
                            format!("More than one task matches “{spoken}”: {}. Which one?", names.join(", "))
                        } else {
                            format!("No task matches “{spoken}”. Tasks: {}.", names.join(", "))
                        }
                    }
                };
                let mut extra = self.choice_extra();
                extra["action"] = json!("show_graph");
                Ok(json!({"reply": reply(Some(&id), text, extra)}))
            }
            "harness_prompt" => {
                let sessions = self.sessions()?;
                let labels = session_labels(&sessions);
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let active = self.settings()["active_session"].as_str().map(str::to_string);
                let ids = action["ids"].as_array().cloned().unwrap_or_default();
                let mut chosen: Vec<&Value> = sessions
                    .iter()
                    .filter(|session| session["running"] == true && ids.contains(&session["id"]))
                    .collect();
                // Several Codex sessions: the active one, else the one here.
                if chosen.len() > 1 {
                    if let Some(session) = chosen.iter().find(|session| session["id"].as_str() == active.as_deref()) {
                        chosen = vec![*session];
                    } else {
                        let local: Vec<&Value> = chosen
                            .iter()
                            .copied()
                            .filter(|session| session["project_id"].as_str() == here.as_deref())
                            .collect();
                        if local.len() == 1 {
                            chosen = local;
                        }
                    }
                }
                let label = |session: &Value| {
                    labels
                        .get(session["id"].as_str().unwrap_or_default())
                        .cloned()
                        .unwrap_or_else(|| session["name"].as_str().unwrap_or("the session").to_string())
                };
                let spoken = action["text"].as_str().unwrap_or_default();
                // A shell takes the command itself: "tell terminal to run ls" types `ls`.
                let shell = chosen.len() == 1
                    && (chosen[0]["profile"] == "terminal"
                        || matches!(chosen[0]["harness"].as_str(), Some("bash" | "zsh" | "sh" | "fish" | "dash" | "ksh")));
                let prompt = match spoken.get(..4) {
                    Some(head) if shell && head.eq_ignore_ascii_case("run ") => spoken[4..].trim(),
                    _ => spoken,
                };
                // "Tell Codex what's left": the open work as Navi records it,
                // for the session's own project.
                let work = match chosen.as_slice() {
                    [session] if !shell => self.work_prompt_for(prompt, session["project_id"].as_str().or(here.as_deref()))?,
                    _ => None,
                };
                let (prompt, shown) = match &work {
                    Some((text, summary)) => (text.as_str(), summary.clone()),
                    None => (prompt, format!("“{prompt}”")),
                };
                let text = match chosen.as_slice() {
                    // A child process owns the terminal: typing would go to it.
                    [session] if matches!(session["state"].as_str(), Some("busy" | "starting" | "created")) => {
                        let name = label(session);
                        self.deliver_when_free(session["id"].as_str().unwrap_or_default(), prompt, &name)?;
                        format!("{name} is busy; I will send {shown} when it is free.")
                    }
                    [session] => {
                        self.inject(session["id"].as_str().unwrap_or_default(), prompt, true)?;
                        format!("Sent to {}: {shown}.", label(session))
                    }
                    [] => "That session is no longer open.".to_string(),
                    many => self.ask_which(action, many, &labels)?,
                };
                Ok(json!({"reply": reply(here.as_deref(), text, self.choice_extra())}))
            }
            "session_screen" => {
                let sessions = self.sessions()?;
                let labels = session_labels(&sessions);
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let active = self.settings()["active_session"].as_str().map(str::to_string);
                let ids = action["ids"].as_array().cloned().unwrap_or_default();
                let mut chosen: Vec<&Value> = sessions
                    .iter()
                    .filter(|session| session["running"] == true && ids.contains(&session["id"]))
                    .collect();
                // Several Codex sessions: the active one, else the only one here.
                if chosen.len() > 1 {
                    if let Some(session) = chosen.iter().find(|session| session["id"].as_str() == active.as_deref()) {
                        chosen = vec![*session];
                    } else {
                        let local: Vec<&Value> = chosen
                            .iter()
                            .copied()
                            .filter(|session| session["project_id"].as_str() == here.as_deref())
                            .collect();
                        if local.len() == 1 {
                            chosen = local;
                        }
                    }
                }
                let text = match chosen.as_slice() {
                    [session] => {
                        let id = session["id"].as_str().unwrap_or_default();
                        let name = labels
                            .get(id)
                            .cloned()
                            .unwrap_or_else(|| session["name"].as_str().unwrap_or("The session").to_string());
                        let glyphs = crate::adapters::prompt_glyphs(session["harness"].as_str().unwrap_or(""));
                        // The input box and key hints are not output.
                        let lines: Vec<String> = self
                            .screen_lines(id, 13)
                            .into_iter()
                            .filter(|line| {
                                let lower = line.to_lowercase();
                                !["for shortcuts", "context left", "esc to interrupt", "to send", "ctrl+"]
                                    .iter()
                                    .any(|hint| lower.contains(hint))
                                    && !glyphs.iter().any(|glyph| line.trim() == *glyph)
                            })
                            .collect();
                        let mut lines = lines;
                        // The prompt waiting at the bottom ("root@host:~#").
                        if lines.last().is_some_and(|line| glyphs.iter().any(|glyph| line.trim_end().ends_with(glyph))) {
                            lines.pop();
                        }
                        let shown = lines[lines.len().saturating_sub(4)..].join(" / ");
                        if shown.is_empty() {
                            format!("{name}'s screen has no text.")
                        } else {
                            let mut shown = shown;
                            if shown.chars().count() > 300 {
                                let skip = shown.chars().count() - 300;
                                shown = format!("…{}", shown.chars().skip(skip).collect::<String>());
                            }
                            format!("{name} shows: “{shown}”.")
                        }
                    }
                    [] => "That session is no longer open.".to_string(),
                    many => self.ask_which(action, many, &labels)?,
                };
                Ok(json!({"reply": reply(here.as_deref(), text, self.choice_extra())}))
            }
            "session_control" => {
                let sessions = self.sessions()?;
                let labels = session_labels(&sessions);
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let ids = action["ids"].as_array().cloned().unwrap_or_default();
                let mut chosen: Vec<&Value> = sessions
                    .iter()
                    .filter(|session| session["running"] == true && ids.contains(&session["id"]))
                    .collect();
                // "Close Codex" with a Codex here and one elsewhere means this one.
                if chosen.len() > 1 {
                    let local: Vec<&Value> = chosen
                        .iter()
                        .copied()
                        .filter(|session| session["project_id"].as_str() == here.as_deref())
                        .collect();
                    if local.len() == 1 {
                        chosen = local;
                    }
                }
                let label = |session: &Value| {
                    labels
                        .get(session["id"].as_str().unwrap_or_default())
                        .cloned()
                        .unwrap_or_else(|| session["name"].as_str().unwrap_or("the session").to_string())
                };
                let text = match chosen.as_slice() {
                    [session] if action["op"] == "restart" => {
                        let result = self.session_restart(&json!({"id": session["id"]}))?;
                        if result["native_id"].as_str().is_some_and(|id| !id.is_empty()) {
                            format!("Restarted {}, continuing its conversation.", label(session))
                        } else {
                            format!("Restarted {}.", label(session))
                        }
                    }
                    [session] => {
                        let target = json!({"id": session["id"]});
                        if action["op"] == "kill" {
                            self.session_kill(&target)?;
                            format!("Closed {}. It stays under Recent.", label(session))
                        } else {
                            self.session_interrupt(&target)?;
                            format!("Interrupted {}.", label(session))
                        }
                    }
                    [] => "That session is no longer open.".to_string(),
                    many => self.ask_which(action, many, &labels)?,
                };
                Ok(json!({"reply": reply(here.as_deref(), text, self.choice_extra())}))
            }
            "session_control_all" => {
                let sessions = self.sessions()?;
                let labels = session_labels(&sessions);
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let filter = spoken_numbers(action["filter"].as_str().unwrap_or_default());
                let singular = filter.strip_suffix("es").filter(|_| filter.ends_with("xes")).or_else(|| filter.strip_suffix('s'));
                let label = |session: &Value| {
                    labels
                        .get(session["id"].as_str().unwrap_or_default())
                        .cloned()
                        .unwrap_or_else(|| session["name"].as_str().unwrap_or("a session").to_string())
                };
                let confirmed = action["confirmed"] == true;
                let chosen: Vec<&Value> = sessions
                    .iter()
                    .filter(|session| session["running"] == true)
                    .filter(|session| match action["ids"].as_array() {
                        Some(ids) if confirmed => ids.contains(&session["id"]),
                        _ => here.is_none() || session["project_id"].as_str() == here.as_deref(),
                    })
                    .filter(|session| {
                        filter.is_empty()
                            || names_session(session, &filter)
                            || singular.is_some_and(|kind| names_session(session, kind))
                    })
                    .filter(|session| {
                        action["op"] == "kill" || matches!(session["state"].as_str(), Some("running" | "busy" | "needs_input"))
                    })
                    .collect();
                let names = chosen.iter().map(|session| label(session)).collect::<Vec<_>>().join(", ");
                let count = |n: usize| format!("{n} session{}", if n == 1 { "" } else { "s" });
                let text = if chosen.is_empty() {
                    match action["op"].as_str() {
                        Some("kill") => "Nothing like that is open here.".to_string(),
                        _ => "Nothing is working right now.".to_string(),
                    }
                } else if action["op"] != "kill" {
                    for session in &chosen {
                        self.session_interrupt(&json!({"id": session["id"]}))?;
                    }
                    format!("Interrupted {names}.")
                } else if confirmed {
                    for session in &chosen {
                        self.session_kill(&json!({"id": session["id"]}))?;
                    }
                    match chosen.len() {
                        1 => format!("Closed {names}. It stays under Recent."),
                        n => format!("Closed {}: {names}. They stay under Recent.", count(n)),
                    }
                } else {
                    // Closing several panes at once is confirmed first.
                    let mut confirm = action.clone();
                    confirm["confirmed"] = json!(true);
                    confirm["ids"] = json!(chosen.iter().map(|session| session["id"].clone()).collect::<Vec<_>>());
                    *self.pending_choice.lock().unwrap() = Some(json!({"time":now(),"action":confirm,"confirm":true,
                        "choices":[{"id":"yes","label":"Yes"},{"id":"no","label":"No"}]}));
                    match chosen.len() {
                        1 => format!("Close {names}? Say yes to confirm."),
                        n => format!("Close {}: {names}? Say yes to confirm.", count(n)),
                    }
                };
                Ok(json!({"reply": reply(here.as_deref(), text, self.choice_extra())}))
            }
            "session_reopen" => {
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let sessions = self.sessions()?;
                let finished = finished_labels(&sessions);
                let ids = action["ids"].as_array().cloned().unwrap_or_default();
                // Newest first; one in this project wins over one elsewhere.
                let candidates: Vec<&Value> = ids
                    .iter()
                    .filter_map(|id| sessions.iter().find(|session| &session["id"] == id))
                    .filter(|session| finished.contains_key(session["id"].as_str().unwrap_or_default()))
                    .collect();
                let chosen = candidates
                    .iter()
                    .find(|session| session["project_id"].as_str() == here.as_deref())
                    .or(candidates.first());
                let text = match chosen {
                    Some(session) => {
                        let result = self.session_restart(&json!({"id": session["id"]}))?;
                        let name = session["name"].as_str().unwrap_or("the session");
                        if result["native_id"].as_str().is_some_and(|id| !id.is_empty()) {
                            format!("Reopened {name}, continuing its conversation.")
                        } else {
                            format!("Reopened {name}.")
                        }
                    }
                    None => match action["open"].as_str() {
                        Some(label) => format!("{label} is already open."),
                        None => "That session was already reopened or removed.".to_string(),
                    },
                };
                Ok(json!({"reply": reply(here.as_deref(), text, json!({}))}))
            }
            "project_list" => {
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let projects = self.projects()?;
                let overview = self.progress_overview()?;
                let percent = |id: &str| {
                    overview["projects"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|row| row["project_id"] == id)
                        .and_then(|row| row["percent"].as_f64())
                        .map(|value| format!(" {}%", value.round()))
                        .unwrap_or_default()
                };
                let names: Vec<String> = projects
                    .iter()
                    .filter_map(|project| {
                        let id = project["id"].as_str()?;
                        Some(format!("{}{}", project["name"].as_str()?, percent(id)))
                    })
                    .collect();
                let current = projects
                    .iter()
                    .find(|project| project["id"].as_str() == here.as_deref())
                    .and_then(|project| project["name"].as_str());
                let text = match (current, names.is_empty()) {
                    (_, true) => "No projects yet. Add one from the project menu.".to_string(),
                    (Some(name), _) => format!("You're in {name}. Projects: {}.", names.join(", ")),
                    (None, _) => format!("Projects: {}.", names.join(", ")),
                };
                Ok(json!({"reply": reply(here.as_deref(), text, json!({}))}))
            }
            "help" => Ok(json!({"reply": reply(None, HELP.into(), json!({}))})),
            "ack" => Ok(json!({"reply": reply(None, action["reply"].as_str().unwrap_or("Okay.").into(),
                json!({}))})),
            "task_add" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let graph = self.graph_get(Some(&id))?;
                let aspects: Vec<Value> = graph["aspects"].as_array().cloned().unwrap_or_default();
                let part_named = |name: &str| {
                    let wanted = loose_title(name);
                    aspects.iter().find(|aspect| loose_title(aspect["title"].as_str().unwrap_or("")) == wanted).cloned()
                };
                // "… to the daemon" names a part only when that part exists;
                // otherwise the words belong to the title.
                let (title, part) = match action["split"].as_array().map(|pair| (pair[0].as_str(), pair[1].as_str())) {
                    Some((Some(title), Some(name))) if part_named(name).is_some() => (title.to_string(), part_named(name)),
                    _ => (action["title"].as_str().unwrap_or_default().to_string(), None),
                };
                let wanted = loose_title(&title);
                let existing = aspects
                    .iter()
                    .flat_map(|aspect| aspect["items"].as_array().into_iter().flatten())
                    .chain(graph["unassigned"].as_array().into_iter().flatten())
                    .any(|task| loose_title(task["title"].as_str().unwrap_or("")) == wanted);
                let part_title = part.as_ref().and_then(|part| part["title"].as_str()).map(str::to_string);
                let text = if existing {
                    format!("“{title}” is already a task.")
                } else {
                    let before = self.last_signal_id()?;
                    let added = self.add_task(&json!({"project_id": id, "title": title,
                        "aspect_id": part.as_ref().map(|part| part["id"].clone())}))?;
                    self.remember_change(&id, before, added["id"].as_str().map(|task| (task, "dropped")),
                        &format!("adding “{title}”"))?;
                    self.event_for(Some(&id), None, "progress", &format!("Added task “{title}”"), json!({"task": added["id"], "source": "user"}));
                    let graph = self.graph_get(Some(&id))?;
                    match part_title {
                        Some(part) => format!("Added “{title}” to {part}. {}", store::progress_sentence(&graph)),
                        None => format!("Added the task “{title}”. {}", store::progress_sentence(&graph)),
                    }
                };
                Ok(json!({"reply": reply(Some(&id), text, json!({"action":"show_graph"}))}))
            }
            "part_add" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let title = action["title"].as_str().unwrap_or_default().to_string();
                let before = self.last_signal_id()?;
                let text = match self.add_aspect(&json!({"project_id": id, "title": title})) {
                    Ok(added) => {
                        self.remember_change(&id, before, None, &format!("adding the part {title}"))?;
                        if let Some(change) = self.last_change.lock().unwrap().as_mut() {
                            change["aspect_added"] = added["id"].clone();
                        }
                        self.event_for(Some(&id), None, "progress", &format!("Added part “{title}”"), json!({"aspect": added["id"], "source": "user"}));
                        format!("Added the part {title}. Say “add a task called … to {title}” to fill it.")
                    }
                    Err(error) if error.contains("already exists") => format!("There is already a part called {title}."),
                    Err(error) => format!("{error}."),
                };
                Ok(json!({"reply": reply(Some(&id), text, json!({"action":"show_graph"}))}))
            }
            "project_cycle" => {
                let projects = self.projects()?;
                let current = project.ok();
                let at = projects.iter().position(|project| current.as_ref().is_some_and(|here| here["id"] == project["id"]));
                let step = if action["back"] == true { projects.len().saturating_sub(1) } else { 1 };
                match projects.len() {
                    0 | 1 => Ok(json!({"reply": reply(None,
                        "There is only one project. Say “add a project” to add another.".into(), json!({}))})),
                    count => {
                        let next = &projects[(at.unwrap_or(0) + step) % count];
                        self.run_fast(clause, &json!({"kind":"project_switch","id":next["id"],"name":next["name"]}), params)
                    }
                }
            }
            "reports" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let source = action["source"].as_str().unwrap_or_default();
                let who = store::capitalise(&store::source_name(source));
                let reports = self.source_reports(&id, source, 3)?;
                let name = project["name"].as_str().unwrap_or("this project");
                let lines: Vec<String> = reports
                    .iter()
                    .map(|report| {
                        let value = report["value"].as_f64().unwrap_or(0.0) * 100.0;
                        let what = match (report["task"].as_str(), report["part"].as_str()) {
                            (Some(task), _) if report["kind"] == "verdict" => format!(
                                "marked “{task}” {}",
                                if value >= 100.0 { "done" } else if value > 0.0 { "in progress" } else { "not done" }
                            ),
                            (Some(task), _) => format!("put “{task}” at {}%", value.round()),
                            (None, Some(part)) => format!("put {part} at {}%", value.round()),
                            (None, None) => format!("put the project at {}%", value.round()),
                        };
                        let evidence: String = report["evidence"].as_str().unwrap_or_default().chars().take(100).collect();
                        let when = relative_time(report["time"].as_u64().unwrap_or(0));
                        if evidence.trim().is_empty() { format!("{what} {when}") } else { format!("{what} {when} (“{}”)", evidence.trim()) }
                    })
                    .collect();
                let text = if lines.is_empty() {
                    format!("{who} has not reported progress on {name}.")
                } else {
                    format!("{who}'s latest reports on {name}: {}.", lines.join("; "))
                };
                Ok(json!({"reply": reply(Some(&id), text, json!({}))}))
            }
            "undo" => {
                let change = self.last_change.lock().unwrap().take();
                let fresh = change.filter(|change| now().saturating_sub(change["time"].as_u64().unwrap_or(0)) < 600_000);
                let Some(change) = fresh else {
                    return Ok(json!({"reply": reply(None, "There is no recent progress change to undo.".into(), json!({}))}));
                };
                let project_id = change["project_id"].as_str().unwrap_or_default().to_string();
                self.undo_change(&change)?;
                let graph = self.graph_get(Some(&project_id))?;
                let text = format!(
                    "Undid {}. {}",
                    change["label"].as_str().unwrap_or("the last change"),
                    store::progress_sentence(&graph)
                );
                Ok(json!({"reply": reply(Some(&project_id), text, json!({"action":"show_graph"}))}))
            }
            "setting" => {
                let key = action["key"].as_str().unwrap_or_default();
                let on = action["on"] == true;
                let text = if key == "microphone" && on && self.isolated {
                    "The microphone is disabled in isolated mode.".to_string()
                } else {
                    let mut settings = self.settings();
                    let was = settings[key] == true;
                    settings[key] = json!(on);
                    self.save_settings(settings, false)?;
                    if key == "microphone" && on {
                        self.spawn_worker("voice")?;
                    }
                    if key == "speak" && !on {
                        // Cut a reply that is being read out now.
                        let _ = std::fs::write(self.runtime.join("voice.cancel"), now().to_string());
                    }
                    match (key, on, was == on) {
                        ("microphone", true, true) => "The microphone is already on.",
                        ("microphone", true, false) => "Microphone on. Say “Navi” to call me.",
                        ("microphone", false, true) => "The microphone is already off.",
                        ("microphone", false, false) => "Microphone off. Turn it back on from the window.",
                        (_, true, true) => "Replies are already spoken.",
                        (_, true, false) => "I will speak my replies.",
                        (_, false, true) => "Replies are already silent.",
                        _ => "Okay, replies stay on screen only.",
                    }
                    .to_string()
                };
                Ok(json!({"reply": reply(None, text, json!({}))}))
            }
            "ui" => Ok(json!({"reply": reply(None, action["reply"].as_str().unwrap_or("Okay.").into(),
                json!({"action": action["action"]}))})),
            "progress_hide" => Ok(json!({"reply": reply(None, "Closed the progress panel.".into(),
                json!({"action":"hide_graph"}))})),
            "session_status" => {
                let sessions = self.sessions()?;
                let projects = self.projects()?;
                let labels = session_labels(&sessions);
                let here = project.as_ref().ok().and_then(|project| project["id"].as_str().map(str::to_string));
                let chosen: Vec<&Value> = sessions
                    .iter()
                    .filter(|session| session["running"] == true)
                    .filter(|session| match action["ids"].as_array() {
                        Some(ids) => ids.contains(&session["id"]),
                        None => here.is_none() || session["project_id"].as_str() == here.as_deref(),
                    })
                    .filter(|session| action["waiting"] != true || session["state"] == "needs_input")
                    .collect();
                let describe = |session: &Value| {
                    let state = match session["state"].as_str().unwrap_or_default() {
                        "needs_input" => "is waiting for your input",
                        "busy" => "is running a command",
                        "running" => "is working",
                        // A harness at rest waits for a prompt; a terminal sits at its shell.
                        "idle" if matches!(
                            crate::adapters::adapter_for(session["harness"].as_str().unwrap_or("")),
                            "claude" | "codex" | "opencode" | "pi" | "agy"
                        ) => "is ready for a prompt",
                        "idle" => "is idle at its shell prompt",
                        "exited" => "has exited",
                        "starting" | "created" => "is starting",
                        _ => "is open",
                    };
                    let label = labels
                        .get(session["id"].as_str().unwrap_or_default())
                        .cloned()
                        .unwrap_or_else(|| session["name"].as_str().unwrap_or("A session").to_string());
                    let mut line = format!("{label} {state}");
                    let held = self
                        .held_prompts
                        .lock()
                        .unwrap()
                        .get(session["id"].as_str().unwrap_or_default())
                        .map_or(0, Vec::len);
                    if held > 0 {
                        line.push_str(&format!(
                            " ({held} prompt{} waiting to be sent)",
                            if held == 1 { "" } else { "s" }
                        ));
                    }
                    // A session from another project says where it is.
                    if let Some(project_id) = session["project_id"].as_str()
                        && here.as_deref() != Some(project_id)
                        && let Some(name) = projects
                            .iter()
                            .find(|project| project["id"] == project_id)
                            .and_then(|project| project["name"].as_str())
                    {
                        line.push_str(&format!(" in {name}"));
                    }
                    line
                };
                let text = match (chosen.len(), action["waiting"] == true) {
                    (0, true) => "Nothing is waiting for your input.".to_string(),
                    (0, false) => match action["absent"].as_str() {
                        Some(name) => format!("{name} is not open. Say “open {name}” to start it."),
                        None => "No harness is open here. Say “open Codex” to start one.".to_string(),
                    },
                    (1, _) => format!("{}.", describe(chosen[0])),
                    _ => format!(
                        "{} sessions: {}.",
                        chosen.len(),
                        chosen.iter().map(|session| describe(session)).collect::<Vec<_>>().join("; ")
                    ),
                };
                Ok(json!({"reply": reply(here.as_deref(), text, json!({}))}))
            }
            "project_switch" => {
                // The active project is window state, so the reply carries the
                // switch to the station; later steps of the command use it too.
                let id = action["id"].as_str().ok_or("Missing project")?.to_string();
                *self.current_project.lock().unwrap() = Some(id.clone());
                let graph = self.graph_get(Some(&id))?;
                let text = format!(
                    "Switched to {}. {}",
                    action["name"].as_str().unwrap_or("the project"),
                    store::progress_sentence(&graph)
                );
                Ok(json!({"project_id": id,
                    "reply": reply(Some(&id), text, json!({"action":"switch_project"}))}))
            }
            "switch" => {
                let target = match action["id"].as_str() {
                    Some(id) => id.to_string(),
                    None => {
                        let here = project.as_ref().ok().and_then(|project| project["id"].as_str());
                        let ids = action["ids"].as_array().cloned().unwrap_or_default();
                        let sessions = self.sessions()?;
                        let local: Vec<&Value> = sessions
                            .iter()
                            .filter(|session| ids.contains(&session["id"]))
                            .filter(|session| session["project_id"].as_str() == here)
                            .collect();
                        match local.as_slice() {
                            [session] => session["id"].as_str().unwrap_or_default().to_string(),
                            _ => {
                                let labels = session_labels(&sessions);
                                let many: Vec<&Value> = sessions
                                    .iter()
                                    .filter(|session| ids.contains(&session["id"]))
                                    .collect();
                                let text = self.ask_which(action, &many, &labels)?;
                                return Ok(json!({"reply": reply(None, text, self.choice_extra())}));
                            }
                        }
                    }
                };
                self.session_target(&json!({"id":target}))?;
                let text = format!("Switched to {}.", action["name"].as_str().unwrap_or("the session"));
                Ok(json!({"id":target,"reply":reply(None, text, json!({}))}))
            }
            "launch" => {
                let project = project?;
                let id = project["id"].as_str().unwrap_or_default().to_string();
                let path = project["path"].as_str().unwrap_or_default();
                // A prompt's own words ("... named after the user") name nothing.
                let name = if action["prompt"].is_string() { None } else { literal_name(clause) };
                // "Open Codex" with Codex already open in this project brings
                // that session forward; "a new Codex" or a name opens another.
                let another = ["new", "another", "fresh", "second", "create", "one more"]
                    .iter()
                    .any(|word| mentioned(clause, word));
                if !another && name.is_none() && action["profile"] != "terminal" {
                    let sessions = self.sessions()?;
                    let open: Vec<&Value> = sessions
                        .iter()
                        .filter(|session| {
                            session["running"] == true
                                && session["profile"] == action["profile"]
                                && session["project_id"].as_str() == Some(id.as_str())
                        })
                        .collect();
                    if let Some(session) = open.iter().max_by_key(|session| session["last_active"].as_i64()) {
                        let target = session["id"].as_str().unwrap_or_default();
                        self.session_target(&json!({"id":target}))?;
                        let label = session["name"].as_str().unwrap_or("The harness");
                        let harness = action["name"].as_str().unwrap_or("it");
                        let text = format!(
                            "{label} is already open in {}; switched to it. Say “open a new {harness}” for another.",
                            project["name"].as_str().unwrap_or("the project")
                        );
                        return Ok(json!({"id":target,"reply":reply(Some(&id), text, json!({"session": target}))}));
                    }
                }
                let work = match action["prompt"].as_str() {
                    Some(prompt) => self.work_prompt_for(prompt, Some(&id))?,
                    None => None,
                };
                let prompt = work.as_ref().map_or(action["prompt"].clone(), |(text, _)| json!(text));
                let session = self.create_session(
                    &json!({"profile":action["profile"],"cwd":path,"name":name,"prompt":prompt}),
                )?;
                let harness = action["name"].as_str().unwrap_or("The harness");
                let project_name = project["name"].as_str().unwrap_or("the project");
                let text = if session["ok"] == false {
                    format!(
                        "{harness} exited right after it started in {project_name}. {}",
                        session["error"].as_str().unwrap_or("")
                    )
                } else if let Some(prompt) = action["prompt"].as_str() {
                    let shown = work.as_ref().map_or(format!("“{prompt}”"), |(_, summary)| summary.clone());
                    match session["prompt"]["ok"].as_bool() {
                        Some(true) => format!("Opened {harness} in {project_name} and sent it: {shown}."),
                        _ => format!(
                            "Opened {harness} in {project_name}, but the prompt was not sent: {}",
                            session["prompt"]["error"].as_str().unwrap_or("it did not become ready")
                        ),
                    }
                } else {
                    format!("Opened {harness} in {project_name}.")
                };
                let mut result = session.clone();
                result["ok"] = json!(true);
                result["reply"] = reply(Some(&id), text, json!({"session": session["id"]}));
                Ok(result)
            }
            other => Err(format!("Unknown fast intent {other}")),
        }
    }

    pub fn run_command(&self, text: &str, params: &Value, dry: bool, generation: u64) -> Result<Value> {
        // "Open Claude and assess the project" is two fast steps. A clause is
        // split on "and" only when every half is a fast intent, so "write a
        // parser and its tests" still reaches the model whole.
        let mut clauses: Vec<&str> = Vec::new();
        for clause in text.split(" then ") {
            let halves: Vec<&str> = clause.split(" and ").collect();
            if halves.len() > 1
                && halves
                    .iter()
                    .all(|half| matches!(self.fast_intent(half), Ok(Some(_))))
            {
                clauses.extend(halves);
            } else {
                clauses.push(clause);
            }
        }
        if clauses.len() > 4 {
            return Err("Use at most four explicit steps in one command".into());
        }
        let pid = params["project_id"]
            .as_str()
            .and_then(|r| self.resolve_project(r).ok())
            .and_then(|p| p["id"].as_str().map(str::to_string));
        // A dry run only previews decisions; it opens no turn in the chat.
        if !dry {
            self.event_for(
                pid.as_deref(),
                None,
                "working",
                "Chief is working",
                json!({"text":text}),
            );
        }
        let (persona, _, _) = self.assistant_target();
        let steps = clauses.len();
        // Any command settles a pending "Which one"; a one-step answer to it
        // runs the narrowed action. A dry run only peeks.
        let pending = if dry {
            self.pending_choice.lock().unwrap().clone()
        } else {
            self.pending_choice.lock().unwrap().take()
        };
        *self.pronoun_target.lock().unwrap() = None;
        let mut choice = pending
            .as_ref()
            .filter(|_| steps == 1)
            .and_then(|pending| resolve_choice(clauses[0], pending));
        // "Switch to acme-api then assess the project": a step that switches
        // project rescopes the steps after it.
        let mut scope = params.clone();
        let mut switched = Value::Null;
        let mut trace = Vec::new();
        let mut reply = Value::Null;
        // How many steps answered, and whether any answer has not reached the
        // feed yet (a model turn posts its own reply as it finishes).
        let mut replies = 0;
        let mut unposted = false;
        let mut shown_action = Value::Null;
        // Each step posts its own reply to the chat; voice speaks them as one.
        let mut spoken: Vec<String> = Vec::new();
        let mut say = |reply: &Value| {
            if let Some(text) = reply["text"].as_str().map(str::trim).filter(|text| !text.is_empty())
                // A step's summary often ends the previous step's reply too.
                && !spoken.iter().any(|earlier| earlier.contains(text))
            {
                spoken.push(text.to_string());
            }
        };
        for (step, clause) in clauses.into_iter().enumerate() {
            let fast = match choice.take() {
                Some(action) => Some(action),
                None => self.fast_intent(clause)?,
            };
            if let Some(action) = fast {
                let decision = json!({"selected":"fast","action":action});
                if dry {
                    trace.push(decision);
                    continue;
                }
                let result = self.run_fast(clause, &action, &scope)?;
                // Later steps' "it" is the session this step opened; the voice
                // target itself stays as the user set it (BUG-081).
                if action["kind"] == "launch"
                    && let Some(session) = result["reply"]["session"].as_str()
                {
                    *self.pronoun_target.lock().unwrap() = Some(session.to_string());
                }
                if matches!(action["kind"].as_str(), Some("project_switch" | "project_cycle")) {
                    scope["project_id"] = result["project_id"].clone();
                    switched = result["project_id"].clone();
                }
                if let Some(r) = result.get("reply").filter(|r| !r.is_null()) {
                    say(r);
                    if r["action"].as_str().is_some_and(|action| !action.is_empty()) {
                        shown_action = r["action"].clone();
                    }
                    reply = r.clone();
                    replies += 1;
                    unposted = true;
                }
                self.event(
                    "command",
                    "Command completed",
                    json!({"decision":decision,"result":result,"step":step,"steps":steps}),
                );
                trace.push(json!({"decision":decision,"result":result}));
                continue;
            }
            // A Navi slash command the fast path could not read gets its usage,
            // not a model's guess.
            if let Some(usage) = navi_slash(clause) {
                return Err(usage.into());
            }
            let decision = match self.decide(clause, generation) {
                Ok(decision) => decision,
                // Without the local decision model the clause is answered as
                // conversation; the fast path above still opens harnesses and
                // handles progress. One warning per ten minutes says why.
                Err(error)
                    if !error.contains("stopped")
                        && self.generation.load(Ordering::SeqCst) == generation =>
                {
                    let last = CHIEF_FALLBACK_WARNED.load(Ordering::SeqCst);
                    if now().saturating_sub(last) > 600_000 {
                        CHIEF_FALLBACK_WARNED.store(now(), Ordering::SeqCst);
                        self.event(
                            "warning",
                            &format!("Local decision model unavailable ({error}). Navi answers as conversation and still opens harnesses and handles progress directly."),
                            json!({"error":error}),
                        );
                    }
                    json!({"selected":"noop","action":{"kind":"noop"},"fallback":error})
                }
                Err(error) => return Err(error),
            };
            if self.generation.load(Ordering::SeqCst) != generation {
                return Err("Command stopped before execution".into());
            }
            let action = &decision["action"];
            if dry {
                trace.push(decision);
                continue;
            }
            // Anything that would not execute a tool (noop, clarify, low
            // confidence) is conversation: ask the OpenCode bridge instead of
            // asking the user to rephrase. Gateway failures propagate as Err —
            // no reply is ever synthesized for a failure (BUG-014).
            let kind = action["kind"].as_str().unwrap_or("");
            let result = if routes_to_conversation(kind) {
                let turn = self.assistant_turn(&scope)?;
                let reply = self.converse(&conversational_text(clause), &turn, generation)?;
                json!({"message":"No action requested","reply":reply})
            } else {
                match kind {
                    "launch" => {
                        let project = self.command_project(clause, &scope)?;
                        let name = literal_name(clause);
                        self.create_session(
                            &json!({"profile":action["profile"],"cwd":project["path"],"name":name}),
                        )?
                    }
                    "switch" => {
                        let target = action["id"].as_str().ok_or("Missing session")?;
                        self.session_target(&json!({"id":target}))?;
                        json!({"id":target})
                    }
                    "resume" => {
                        self.session_resume(&json!({"id":action["id"]}))?
                    }
                    "browser" => {
                        if self.isolated {
                            return Err(
                                "Opening the user's browser is disabled in isolated mode".into()
                            );
                        }
                        Command::new("zen")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .map_err(|e| e.to_string())?;
                        json!({"message":"Zen launch requested"})
                    }
                    "url" => {
                        if self.isolated {
                            return Err(
                                "Opening the user's browser is disabled in isolated mode".into()
                            );
                        }
                        let url = action["url"].as_str().ok_or("Missing favorite URL")?;
                        if !url.starts_with("https://") && !url.starts_with("http://") {
                            return Err("Favorite URL must be http or https".into());
                        }
                        Command::new("zen")
                            .args(["--new-tab", url])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .map_err(|e| e.to_string())?;
                        json!({"message":"Browser favorite launch requested","url":url})
                    }
                    "browser_action" => {
                        self.browser_action(action["action"].clone(), generation)?
                    }
                    "desktop_action" => self.desktop_request(&action["action"], generation)?,
                    _ => return Err("Unregistered action kind".into()),
                }
            };
            if result["ok"] == false {
                return Err(result["error"].as_str().unwrap_or("Action failed").into());
            }
            if let Some(r) = result.get("reply").filter(|r| !r.is_null()) {
                say(r);
                reply = r.clone();
                replies += 1;
            } else if let Some(text) = action_summary(kind, action, &result) {
                // An action the model chose answers in one line, so its turn
                // ends with a reply like any other.
                let r = json!({"text":text,"persona":persona,"source":"navi","project_id":pid});
                say(&r);
                reply = r;
                replies += 1;
                unposted = true;
            }
            self.event(
                "command",
                "Command completed",
                json!({"decision":decision,"result":result,"step":step,"steps":steps}),
            );
            trace.push(json!({"decision":decision,"result":result}));
        }
        if !spoken.is_empty() && reply.is_object() {
            reply["text"] = json!(spoken.join(" "));
        }
        if !shown_action.is_null()
            && reply.is_object()
            && reply["action"].as_str().is_none_or(str::is_empty)
        {
            reply["action"] = shown_action;
        }
        // The last step's reply may be about something else ("…then assess
        // the project"), so the switch travels in its own field.
        if !switched.is_null() && reply.is_object() {
            reply["switch_project"] = switched;
        }
        // The reply belongs beside its question, in the project it was asked
        // in, even when it is about another one ("open Codex in acme-api");
        // a switch moves the station only after the row is stored.
        if let Some(asked_in) = pid.as_deref()
            && reply.is_object()
            && reply["project_id"].as_str() != Some(asked_in)
        {
            reply["about_project"] = reply["project_id"].clone();
            reply["project_id"] = json!(asked_in);
        }
        // One reply per command reaches the chat: every step's answer joined,
        // unless the only answer was a model turn that already posted itself.
        if !dry && reply.is_object() && (unposted || replies > 1) {
            let project = reply["project_id"].as_str().map(str::to_string).or(pid);
            let text = reply["text"].as_str().unwrap_or_default().to_string();
            self.event_for(project.as_deref(), None, "reply", &text, reply.clone());
        }
        Ok(json!({"trace":trace,"dry_run":dry,"reply":reply}))
    }
}

struct AssistantTurn {
    id: String,
    path: String,
    context: String,
    attachments: Vec<String>,
}

/// A gateway failure: `stopped` marks a user Stop so callers can return a
/// `stopped`/`interrupted` reply instead of an error (BUG-007).
struct GatewayFailure {
    message: String,
    stopped: bool,
    interrupted: bool,
}

impl GatewayFailure {
    fn new(message: impl Into<String>, interrupted: bool) -> Self {
        let message = message.into();
        Self {
            stopped: message == "stopped",
            interrupted,
            message,
        }
    }
}

/// What one gateway stdout line means for the daemon (BUG-001). Pure mapping;
/// `map_gateway_line` never touches process state so fixtures can test it.
enum GatewayLine {
    Delta(String),
    Reasoning(String, bool),
    ToolStatus {
        id: String,
        name: Option<String>,
        input: Value,
        progress: Value,
    },
    ToolFinal {
        id: String,
        name: Option<String>,
        status: String,
        input: Value,
        output: Value,
        exit: Value,
    },
    Permission {
        id: Value,
        tool: Option<String>,
        input: Value,
    },
    Warning(String),
    Error(String),
    Done(Box<Value>),
}

/// Per-turn stream state: accumulated reply text, deduplicated tool finals,
/// and whether real reasoning has been seen (only that may emit `thinking`).
#[derive(Default)]
struct GatewayTurn {
    accumulated: String,
    finished_tools: HashSet<String>,
    reasoned: bool,
}

impl GatewayTurn {
    /// Merge stream semantics into a mapped line; returns false when the line
    /// must produce no event (duplicate tool final — exactly one per id).
    fn record(&mut self, event: &mut GatewayLine) -> bool {
        match event {
            GatewayLine::Delta(text) => self.accumulated.push_str(text),
            GatewayLine::Reasoning(_, first) => {
                *first = !self.reasoned;
                self.reasoned = true;
            }
            GatewayLine::ToolFinal { id, .. }
                if !self.finished_tools.insert(id.clone()) =>
            {
                return false;
            }
            GatewayLine::ToolFinal { .. } => {}
            _ => {}
        }
        true
    }
}

/// Map one raw stdout line from the gateway to a `GatewayLine` (BUG-001).
/// Unparseable or unknown lines return None.
fn map_gateway_line(line: &str) -> Option<GatewayLine> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value["type"].as_str()? {
        "delta" => Some(GatewayLine::Delta(value["text"].as_str()?.to_string())),
        "reasoning" => Some(GatewayLine::Reasoning(
            value["text"].as_str()?.to_string(),
            false,
        )),
        "tool_status" => Some(GatewayLine::ToolStatus {
            id: value["id"].as_str()?.to_string(),
            name: value["name"].as_str().map(str::to_string),
            input: value["input"].clone(),
            progress: value["progress"].clone(),
        }),
        "tool" => Some(GatewayLine::ToolFinal {
            id: value["id"].as_str()?.to_string(),
            name: value["name"].as_str().map(str::to_string),
            status: value["status"].as_str().unwrap_or("completed").to_string(),
            input: value["input"].clone(),
            output: value["output"].clone(),
            exit: value["exit"].clone(),
        }),
        "permission" => Some(GatewayLine::Permission {
            id: value["id"].clone(),
            tool: value["tool"].as_str().map(str::to_string),
            input: value["input"].clone(),
        }),
        "warning" => Some(GatewayLine::Warning(value["text"].as_str()?.to_string())),
        "error" => Some(GatewayLine::Error(
            value["text"].as_str().unwrap_or("Assistant error").to_string(),
        )),
        "done" => Some(GatewayLine::Done(Box::new(value))),
        _ => None,
    }
}

/// Shape/size checks shared by `command` and `prompt` before a request may be
/// queued: ≤4 files, ≤2 MiB each, no control characters in names (BUG-005).
pub(crate) fn validate_attachments(params: &Value) -> Result<()> {
    let Some(items) = params.as_array() else {
        if params.is_null() {
            return Ok(());
        }
        return Err("attachments must be an array".into());
    };
    if items.len() > 4 {
        return Err("At most 4 attachments per prompt".into());
    }
    for item in items {
        let name = item["name"].as_str().unwrap_or_default();
        if !name.is_empty() && (name.len() > 120 || name.chars().any(char::is_control)) {
            return Err("Invalid attachment name".into());
        }
        match (item["path"].as_str(), item["data"].as_str()) {
            (Some(_), _) | (None, Some(_)) => {}
            _ => return Err("Attachment needs a path or data".into()),
        }
        if let Some(data) = item["data"].as_str() {
            let encoded = data.chars().filter(|c| !c.is_whitespace()).count();
            if encoded * 3 / 4 > 2 * 1024 * 1024 {
                return Err("Attachment exceeds 2 MiB".into());
            }
        }
        if let Some(path) = item["path"].as_str()
            && path.chars().any(char::is_control)
        {
            return Err("Invalid attachment path".into());
        }
    }
    Ok(())
}

/// Strict base64 decode (standard alphabet, optional whitespace).
fn b64_decode(data: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let clean: Vec<u8> = data.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    for chunk in clean.chunks(4) {
        if chunk.len() < 4 {
            return None;
        }
        let mut acc = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            acc <<= 6;
            if *byte == b'=' {
                if i < 2 || chunk[i..].iter().any(|b| *b != b'=') {
                    return None;
                }
                continue;
            }
            acc |= match byte {
                b'A'..=b'Z' => (*byte - b'A') as u32,
                b'a'..=b'z' => (*byte - b'a' + 26) as u32,
                b'0'..=b'9' => (*byte - b'0' + 52) as u32,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            };
        }
        let payload = &chunk[..chunk.iter().position(|b| *b == b'=').unwrap_or(4)];
        if payload.len() >= 2 {
            out.push((acc >> 16) as u8);
        }
        if payload.len() >= 3 {
            out.push((acc >> 8) as u8);
        }
        if payload.len() == 4 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// Every task with its part title, open ones first by how far behind their
/// part is: in progress, then to do, then blocked.
fn open_tasks(graph: &Value) -> Vec<(String, String, f64, String)> {
    let mut open: Vec<(String, String, f64, String)> = graph["aspects"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|aspect| {
            let part = aspect["title"].as_str().unwrap_or_default().to_string();
            let percent = aspect["percent"].as_f64().unwrap_or(0.0);
            aspect["items"].as_array().into_iter().flatten().map(move |item| (item.clone(), part.clone(), percent))
        })
        .chain(graph["unassigned"].as_array().into_iter().flatten().map(|item| (item.clone(), String::new(), 100.0)))
        .filter(|(item, _, _)| item["state"] != "done")
        .map(|(item, part, percent)| {
            (item["title"].as_str().unwrap_or("Untitled").to_string(), part, percent,
                item["state"].as_str().unwrap_or("todo").to_string())
        })
        .collect();
    let rank = |state: &str| match state {
        "in_progress" => 0,
        "blocked" => 2,
        _ => 1,
    };
    open.sort_by(|left, right| rank(&left.3).cmp(&rank(&right.3)).then(left.2.total_cmp(&right.2)));
    open
}

fn named_task(title: &str, part: &str) -> String {
    if part.is_empty() { format!("“{title}”") } else { format!("“{title}” in {part}") }
}

/// "What's the next task": the task to pick up, and that task (title, part)
/// for a prompt.
fn next_task(graph: &Value) -> (String, Option<(String, String)>) {
    let open = open_tasks(graph);
    match open.iter().find(|task| task.3 != "blocked") {
        Some((title, part, _, state)) => {
            let lead = if state == "in_progress" { "Already in progress" } else { "Next" };
            (format!("{lead}: {}.", named_task(title, part)), Some((title.clone(), part.clone())))
        }
        None => {
            let behind = graph["aspects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|aspect| Some((aspect["title"].as_str()?, aspect["percent"].as_f64()?)))
                .filter(|(_, percent)| *percent < 100.0)
                .min_by(|left, right| left.1.total_cmp(&right.1));
            let blocked = if open.is_empty() { String::new() } else { format!(" {} blocked.", open.len()) };
            match behind {
                Some((part, percent)) => (
                    format!("No open task to pick up.{blocked} The part furthest behind is {part} at {}%.", percent.round()),
                    Some((String::new(), part.to_string())),
                ),
                None => (format!("No open task to pick up.{blocked}"), None),
            }
        }
    }
}

/// "What's in progress": the tasks being worked on.
fn active_sentence(graph: &Value) -> String {
    let active: Vec<String> = open_tasks(graph)
        .into_iter()
        .filter(|task| task.3 == "in_progress")
        .map(|(title, part, _, _)| named_task(&title, &part))
        .collect();
    match active.len() {
        0 => "No task is marked in progress.".to_string(),
        1 => format!("In progress: {}.", active[0]),
        count => format!("{count} tasks in progress: {}.", active.join(", ")),
    }
}

/// "How many tasks are left": the open count by state.
fn count_sentence(graph: &Value) -> String {
    let counts = &graph["tasks"];
    let number = |key: &str| counts[key].as_u64().unwrap_or(0);
    let total = number("total");
    if total == 0 {
        return "There are no tasks yet. Say “add a task called …” to add one.".to_string();
    }
    let open = total.saturating_sub(number("done"));
    let mut sentence = format!("{open} of {total} tasks {} open", if open == 1 { "is" } else { "are" });
    let detail: Vec<String> = [("in_progress", "in progress"), ("blocked", "blocked")]
        .iter()
        .filter(|(key, _)| number(key) > 0)
        .map(|(key, name)| format!("{} {name}", number(key)))
        .collect();
    if !detail.is_empty() {
        sentence.push_str(&format!(" ({})", detail.join(", ")));
    }
    sentence.push('.');
    sentence
}

/// "List the parts": every part with its percent.
fn parts_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("This project");
    let parts: Vec<String> = graph["aspects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|aspect| {
            let title = aspect["title"].as_str().unwrap_or("Untitled");
            match aspect["percent"].as_f64() {
                Some(percent) => format!("{title} {}%", percent.round()),
                None => format!("{title} (no estimate)"),
            }
        })
        .collect();
    match parts.len() {
        0 => format!("{name} has no parts yet. Say “add a part called …” to add one."),
        1 => format!("{name} has one part: {}.", parts[0]),
        count => format!("{name} has {count} parts: {}.", parts.join(", ")),
    }
}

/// "When was the last estimate", "how confident is it": where the headline
/// comes from, the newest estimate, and the confidence.
fn estimate_sentence(graph: &Value) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("This project");
    let Some(percent) = graph["percent"].as_f64() else {
        return format!("{name} has no estimate yet. Say “assess the project” and I will ask a harness to judge it.");
    };
    let mut sentence = format!("{name} is at {}%", percent.round());
    match graph["basis"]["kind"].as_str() {
        Some("parts") => sentence.push_str(" as the weighted mean of its parts"),
        Some("estimate") => sentence.push_str(" from the newest whole-project estimate"),
        _ => {}
    }
    sentence.push('.');
    let estimate = &graph["estimate"];
    if let (Some(value), Some(time)) = (estimate["percent"].as_f64(), estimate["time"].as_u64()) {
        let who = match estimate["source"].as_str().unwrap_or("assistant") {
            "user" => "You".to_string(),
            source => store::source_name(source),
        };
        sentence.push_str(&format!(" {who} last put the whole project at {}%, {}", value.round(), relative_time(time)));
        if let Some(evidence) = estimate["evidence"].as_str().map(str::trim).filter(|text| !text.is_empty()) {
            let short: String = evidence.chars().take(120).collect();
            sentence.push_str(&format!(" (“{short}”)"));
        }
        sentence.push('.');
    }
    if let Some(confidence) = graph["confidence"].as_f64() {
        sentence.push_str(&format!(" Confidence is {}%.", (confidence * 100.0).round()));
    }
    match graph["commits_since"].as_i64().unwrap_or(0) {
        0 => {}
        1 => sentence.push_str(" 1 commit landed since; say “reassess” to update it."),
        commits => sentence.push_str(&format!(" {commits} commits landed since; say “reassess” to update it.")),
    }
    sentence
}

/// Whether a prompt asks Navi to hand over the recorded work: `Some(true)`
/// for the next task, `Some(false)` for everything open.
fn work_request(prompt: &str) -> Option<bool> {
    let text = loose_title(prompt);
    let text = text.strip_prefix("to ").unwrap_or(&text);
    let text = text.strip_suffix(" please").unwrap_or(text);
    match text {
        "what s left" | "what is left" | "whats left" | "what remains" | "what needs doing" | "what s left to do"
        | "the open tasks" | "the open work" | "the remaining work" | "work on what s left" | "do what s left"
        | "finish what s left" | "the todo list" | "the to do list" => Some(false),
        "the next task" | "next task" | "do the next task" | "work on the next task" | "what to do next"
        | "the next thing" | "do the next thing" | "start the next task" | "pick up the next task" => Some(true),
        _ => None,
    }
}

/// A prompt that hands a harness the open work, as Navi records it.
fn work_prompt(graph: &Value, next_only: bool) -> String {
    let name = graph["project"]["name"].as_str().unwrap_or("this project");
    let report = "Record progress as you go with the navi_track_task and navi_report_progress tools.";
    if next_only {
        return match next_task(graph).1 {
            Some((title, part)) if !title.is_empty() => {
                format!("Work on the task {} for {name}. {report}", named_task(&title, &part))
            }
            Some((_, part)) => format!("Work on the {part} part of {name}, which is furthest behind. {report}"),
            None => format!("{name} has no open work recorded. Check what is missing and add tasks for it. {report}"),
        };
    }
    format!("{} Pick the most important item and do it. {report}", store::remaining_sentence(graph))
}

/// "2 minutes ago" style activity stamp for the context block.
fn relative_time(at: u64) -> String {
    let elapsed = now().saturating_sub(at) / 1000;
    for (unit, seconds) in [
        ("day", 86_400_u64),
        ("hour", 3_600),
        ("minute", 60),
    ] {
        if elapsed >= seconds {
            let value = elapsed / seconds;
            return format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" });
        }
    }
    "just now".into()
}

/// Trim the context block to ≤600 bytes on a char boundary.
fn cap_block(text: &str) -> String {
    if text.len() <= 600 {
        return text.to_string();
    }
    let mut end = 600;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

// Model action kinds that actually execute a tool. Everything else (noop,
// clarify, low confidence, an unexpected kind) runs no tool and is answered as
// conversation by the OpenCode bridge instead of a clarification prompt.
fn routes_to_conversation(kind: &str) -> bool {
    !matches!(
        kind,
        "launch" | "switch" | "resume" | "browser" | "url" | "browser_action" | "desktop_action"
    )
}

// Strip a leading wake phrase before an utterance goes to the conversation
// bridge: "hey navi, what is X" and "navi what is X" both become "what is X".
// A bare wake word keeps the original text so the bridge still gets a request.
/// "It's 4:05 PM." or "It's Thursday, November 5, 2026.", in local time.
fn clock_sentence(date: bool) -> String {
    let seconds = (now() / 1000) as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return "I cannot read the clock.".to_string();
    }
    if date {
        const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
        const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August",
            "September", "October", "November", "December"];
        return format!(
            "It's {}, {} {}, {}.",
            DAYS[tm.tm_wday.clamp(0, 6) as usize],
            MONTHS[tm.tm_mon.clamp(0, 11) as usize],
            tm.tm_mday,
            tm.tm_year + 1900
        );
    }
    let hour = match tm.tm_hour % 12 {
        0 => 12,
        hour => hour,
    };
    format!("It's {hour}:{:02} {}.", tm.tm_min, if tm.tm_hour < 12 { "AM" } else { "PM" })
}

fn conversational_text(text: &str) -> String {
    // Wake words and spoken fillers lead the request: "Hey Navi. Stop Codex.",
    // "um, open a terminal", "okay Navi, what's left".
    const LEADS: [&str; 19] = ["hey navi", "okay navi", "ok navi", "navi", "umm", "um", "uhh",
        "uh", "erm", "hmm", "okay", "ok", "alright", "well", "so", "now", "right", "go ahead and",
        "and"];
    let separator = |c: char| c.is_whitespace() || matches!(c, ',' | ':' | '.' | '!');
    let mut current = text.trim();
    'strip: loop {
        for lead in LEADS {
            let Some(head) = current.get(..lead.len()) else {
                continue;
            };
            let rest = &current[lead.len()..];
            // Require a word boundary so "navigation tips" keeps its text.
            if !head.eq_ignore_ascii_case(lead) || !(rest.is_empty() || rest.starts_with(separator)) {
                continue;
            }
            let stripped = rest.trim_start_matches(separator).trim();
            if stripped.is_empty() {
                return current.to_string();
            }
            current = stripped;
            continue 'strip;
        }
        return current.to_string();
    }
}

// Deterministic voice-command grammar for persona changes, independent of the
// decision model: "switch to navi mode", "be concise", "use friendly
// persona". Trailing punctuation is ignored; unsupported phrasing returns None.
fn persona_target(text: &str) -> Option<String> {
    let lower = text.trim().to_lowercase();
    let target = lower
        .strip_prefix("switch to ")
        .map(|rest| rest.strip_suffix(" mode").unwrap_or(rest))
        .or_else(|| lower.strip_prefix("be "))
        .or_else(|| {
            lower
                .strip_prefix("use ")
                .map(|rest| rest.strip_suffix(" persona").unwrap_or(rest))
        })?
        .trim()
        .trim_end_matches(['.', ',', '!', '?'])
        .trim();
    if target.is_empty() || target.len() > 40 || target.split_whitespace().count() > 3 {
        return None;
    }
    Some(target.to_string())
}

// Match whole words/phrases, so a short profile such as Pi cannot match
// arbitrary words like "picture" or a session prefix shared by two names.
fn mentioned(text: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    if name.is_empty() {
        return false;
    }
    let text = text.to_lowercase();
    text.match_indices(&name).any(|(index, _)| {
        text[..index]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
            && text[index + name.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric())
    })
}

fn negated(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if [
        "what does ",
        "what would ",
        "explain ",
        "tell me about ",
        "for example ",
        "maybe later ",
        "perhaps later ",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    [
        "don't",
        "do not",
        "never",
        "cancel",
        "stop",
        "keep the current",
        "keep current",
    ]
    .iter()
    .any(|name| mentioned(text, name))
}

fn immediate_request(text: &str) -> bool {
    let mut lower = text.trim().to_lowercase();
    for prefix in [
        "hey navi, ",
        "hey navi ",
        "navi, ",
        "please ",
        "could you ",
        "can you ",
        "would you ",
        "i want you to ",
        "i want to ",
        "let's ",
        "lets ",
    ] {
        if let Some(command) = lower.strip_prefix(prefix) {
            lower = command.to_owned();
        }
    }
    let literal_instruction = lower
        .split(" called ")
        .next()
        .unwrap()
        .split(" named ")
        .next()
        .unwrap();
    if ["later", "tomorrow", "next week", "next hour"]
        .iter()
        .any(|word| mentioned(literal_instruction, word))
    {
        return false;
    }
    [
        "open",
        "launch",
        "start",
        "create",
        "switch",
        "go back",
        "go to",
        "jump to",
        "change to",
        "move to",
        "return",
        "select",
        "resume",
        "focus",
        "attach",
        "click",
        "press",
        "activate",
        "scroll",
        "next window",
        "bring up",
        "fire up",
        "spin up",
        "boot up",
        "take me to",
        "show me",
        "show",
        "pull up",
        "new",
    ]
    .iter()
    .any(|verb| {
        lower == *verb
            || lower
                .strip_prefix(verb)
                .is_some_and(|suffix| suffix.starts_with(' '))
    })
}

fn project_for(text: &str, settings: &Value) -> Result<Value> {
    let projects = settings["projects"].as_array().ok_or("Missing projects")?;
    let lower = text.to_lowercase();
    if let Some((_, suffix)) = lower.split_once(" in ") {
        let target = suffix
            .split(" called ")
            .next()
            .unwrap()
            .split(" named ")
            .next()
            .unwrap()
            .trim()
            .trim_end_matches(['.', ',', '!'])
            .trim_end_matches(" please")
            .trim()
            .trim_end_matches(['.', ',', '!', '?'])
            .trim();
        let matched = projects
            .iter()
            .filter(|project| {
                project["name"]
                    .as_str()
                    .is_some_and(|name| name.to_lowercase() == target)
            })
            .collect::<Vec<_>>();
        if matched.len() != 1 {
            return Err("Specify one registered project; no directory was guessed".into());
        }
        Ok(matched[0]["path"].clone())
    } else {
        Ok(projects.first().ok_or("Add a project in settings")?["path"].clone())
    }
}

/// "progress" as the subject, not a modifier: "the progress of Navi" counts,
/// "the progress bar" does not.
fn progress_subject(lower: &str) -> bool {
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|word| !word.is_empty())
        .collect();
    words.iter().enumerate().all(|(index, word)| {
        !matches!(*word, "progress" | "completion")
            || words.get(index + 1).is_none_or(|next| {
                matches!(
                    *next,
                    "of" | "on" | "for" | "in" | "so" | "now" | "today" | "overall" | "report"
                        | "percentage" | "percent"
                        | "status" | "check" | "please" | "is" | "looks" | "look" | "again"
                        | "graph" | "panel" | "view"
                )
            })
    })
}

/// "mark barge-in done", "mark the streaming TTS task as in progress",
/// "check off barge-in", "reopen barge-in": a task title as spoken and the
/// state the user gave it.
fn parse_task_state(lower: &str) -> Option<(String, &'static str)> {
    let text = lower.trim();
    let (title, state) = if let Some(rest) = text.strip_prefix("check off ") {
        (rest, "done")
    } else if let Some(rest) = text.strip_prefix("reopen ") {
        (rest, "todo")
    } else if let Some(rest) = ["drop the task ", "drop task ", "drop ", "remove the task ", "remove task ",
        "delete the task ", "delete task "]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))
    {
        (rest, "dropped")
    } else {
        let rest = text.strip_prefix("mark ")?;
        // Longer suffixes first: "not done" also ends in "done".
        const STATES: [(&str, &str); 20] = [
            (" as not done", "todo"),
            (" not done", "todo"),
            (" as to do", "todo"),
            (" to do", "todo"),
            (" as todo", "todo"),
            (" todo", "todo"),
            (" as in progress", "in_progress"),
            (" in progress", "in_progress"),
            (" as started", "in_progress"),
            (" started", "in_progress"),
            (" as blocked", "blocked"),
            (" blocked", "blocked"),
            (" as done", "done"),
            (" done", "done"),
            (" as completed", "done"),
            (" completed", "done"),
            (" as complete", "done"),
            (" complete", "done"),
            (" as finished", "done"),
            (" finished", "done"),
        ];
        STATES
            .iter()
            .find_map(|(suffix, state)| rest.strip_suffix(suffix).map(|title| (title, *state)))?
    };
    let title = title.trim();
    let title = title.strip_prefix("the ").unwrap_or(title);
    let title = title.strip_suffix(" task").unwrap_or(title);
    let title = title.trim().trim_matches(['"', '“', '”', '\'']).trim();
    (!title.is_empty() && title.split_whitespace().count() <= 8).then(|| (title.to_string(), state))
}

/// The text after the first of `prefixes` (matched without case), in its
/// original casing, without quotes or trailing punctuation.
fn after_prefix<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let text = text.trim().trim_end_matches(['.', '!', '?']).trim();
    let lower = text.to_ascii_lowercase();
    let start = prefixes.iter().find_map(|prefix| lower.starts_with(prefix).then_some(prefix.len()))?;
    let rest = text[start..].trim().trim_start_matches([':', ',']).trim();
    Some(rest.trim_matches(['"', '“', '”', '\'']).trim())
}

/// "Add a task called fix login to the daemon", "new task: write docs": the
/// whole title, and the title and part when the words after the last "to",
/// "in", "under" or "for" may name a part (the caller checks it exists).
fn parse_task_add(text: &str) -> Option<(String, Option<(String, String)>)> {
    let rest = after_prefix(text, &["add a new task called ", "add a new task named ", "add a task called ",
        "add a task named ", "add a new task ", "add a task ", "add task ", "add task:", "add a todo ",
        "new task called ", "new task:", "new task ", "create a task called ", "create a task named ", "create a task ", "create task "])?;
    if rest.is_empty() || rest.split_whitespace().count() > 12 {
        return None;
    }
    let lower = rest.to_ascii_lowercase();
    let split = [" to the ", " in the ", " under the ", " for the ", " to ", " in ", " under ", " for "]
        .iter()
        .filter_map(|marker| lower.rfind(marker).map(|at| (at, marker.len())))
        .max_by_key(|(at, len)| (*at, *len))
        .map(|(at, len)| {
            let part = rest[at + len..].trim();
            let part = part.strip_suffix(" part").unwrap_or(part);
            (rest[..at].trim().to_string(), part.to_string())
        })
        .filter(|(head, part)| !head.is_empty() && !part.is_empty());
    Some((rest.to_string(), split))
}

/// "Add a part called docs", "create a part named storage": a part title.
fn parse_part_add(text: &str) -> Option<String> {
    let title = after_prefix(text, &["add a new part called ", "add a new part named ", "add a part called ",
        "add a part named ", "add a new part ", "add a part ", "add part ", "new part called ", "new part ",
        "create a part called ", "create a part named ", "create a part ", "create part "])?;
    (!title.is_empty() && title.split_whitespace().count() <= 6).then(|| title.to_string())
}

/// "Barge-in is done", "we finished barge-in", "voice is blocked": a statement
/// about one task or part. The caller keeps it only when a task or part by
/// that name exists, so "the build is done" stays conversation.
fn parse_task_statement(lower: &str) -> Option<(String, &'static str)> {
    let text = lower.trim();
    if ["is ", "are ", "has ", "have ", "did ", "does ", "do ", "what", "how", "when", "why", "which", "who"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return None;
    }
    let finished = ["we finished ", "i finished ", "finished ", "we're done with ", "we are done with ",
        "i'm done with ", "done with ", "we completed ", "i completed "]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix));
    let started = ["start working on ", "started working on ", "i'm working on ", "i am working on ",
        "we're working on ", "we are working on ", "working on ", "i started ", "we started ", "starting on "]
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix));
    let (title, state) = match (finished, started) {
        (Some(rest), _) => (rest, "done"),
        (_, Some(rest)) => (rest, "in_progress"),
        _ => {
            const STATES: [(&str, &str); 8] = [
                (" is not done", "todo"),
                (" is done", "done"),
                (" is finished", "done"),
                (" is complete", "done"),
                (" is completed", "done"),
                (" is blocked", "blocked"),
                (" is in progress", "in_progress"),
                (" is started", "in_progress"),
            ];
            STATES.iter().find_map(|(suffix, state)| text.strip_suffix(suffix).map(|title| (title, *state)))?
        }
    };
    let title = title.trim();
    let title = title.strip_prefix("the ").unwrap_or(title);
    let title = title.strip_suffix(" task").or_else(|| title.strip_suffix(" part")).unwrap_or(title).trim();
    let whole = matches!(title, "it" | "this" | "that" | "everything" | "project" | "this project"
        | "we" | "all" | "i");
    (!whole && !title.is_empty() && title.split_whitespace().count() <= 6).then(|| (title.to_string(), state))
}

/// The one-line answer to an action the decision model chose, when the action
/// itself produced no reply.
fn action_summary(kind: &str, action: &Value, result: &Value) -> Option<String> {
    let name = result["name"]
        .as_str()
        .or_else(|| action["name"].as_str())
        .filter(|name| !name.is_empty());
    Some(match kind {
        "launch" => format!("Opened {}.", name.unwrap_or("the harness")),
        "switch" => format!("Switched to {}.", name.unwrap_or("that session")),
        "resume" => format!("Resumed {}.", name.unwrap_or("the session")),
        "browser" => "Opened Zen.".to_string(),
        "url" => format!("Opened {}.", result["url"].as_str().unwrap_or("the page")),
        _ => {
            let message = result["message"].as_str().filter(|text| !text.is_empty())?;
            format!("{}.", message.trim_end_matches('.'))
        }
    })
}

/// Navi's own slash commands (answered by the fast path, never sent to the
/// assistant), as the usage line shown when one cannot be read.
/// "What changed today": progress questions about the last day.
/// "Is the estimate up to date?": answered with the headline, which names the
/// commits made since it.
const STALE_PHRASES: [&str; 7] = [
    "estimate up to date", "estimate current", "estimate out of date", "estimate stale",
    "commits since", "commits landed since", "up to date estimate",
];

/// Progress questions with their own answer, checked in order.
const TOPIC_PHRASES: [(&str, &[&str]); 6] = [
    ("estimate", &["last estimate", "latest estimate", "newest estimate", "the estimate based", "how confident",
        "confidence", "why is it", "why is the project", "why is navi", "where does the number",
        "where does the percent", "what is it based on", "what's it based on", "how was it estimated"]),
    ("next", &["next task", "next thing to do", "what to do next"]),
    ("count", &["how many tasks", "how many open tasks", "number of tasks"]),
    ("active", &["in progress", "being worked on", "who is working on", "what's being done", "currently working on"]),
    ("parts", &["list the parts", "list parts", "what parts", "which parts are there", "the parts are",
        "show me the parts", "all the parts", "every part"]),
    ("tasks", &["open tasks", "list the tasks", "list tasks", "show me the tasks", "what tasks", "which tasks",
        "the task list", "the checklist", "all the tasks"]),
];

/// "What's blocked": the blocked tasks.
const BLOCKED_PHRASES: [&str; 11] = ["what's blocking", "what is blocking", "whats blocking",
    "what's blocked", "what is blocked", "whats blocked", "anything blocked", "any blockers",
    "the blockers", "which tasks are blocked", "what tasks are blocked"];

/// "What's done": the finished parts and tasks.
const DONE_PHRASES: [&str; 12] = ["what's done", "what is done", "whats done", "what's finished",
    "what is finished", "what have we finished", "what have we done", "what's been done",
    "what has been done", "parts are done", "parts are finished", "tasks are done"];

const RECENT_PHRASES: [&str; 12] = ["what changed", "what's changed", "what has changed", "what's new",
    "whats new", "progress today", "today's progress", "what happened today", "what did we do today",
    "what got done", "since yesterday", "any progress"];

/// What Navi answers itself, without a model; spoken too, so it stays short.
const HELP: &str = "Without a model I can: open a harness (“open Codex”, “open a new Claude Code”), switch project (“go to acme-api”, “which project am I in”), report progress (“how far along is the project”, “what's left”, “what's blocked”, “what changed today”), assess it (“assess the project”), record progress (“set voice to 80%”, “mark barge-in done”, “add a task called fix login to the daemon”, “undo”), check sessions (“is Codex done”, “what's running”, “what did Codex say”), give one a task (“tell Codex to run the tests”, “give Codex the next task”; a closed harness opens first, a busy one gets it when free), stop, close or reopen one (“stop Codex”, “close terminal 2”, “reopen Codex”, “stop everything”), show or close the progress panel, and turn the microphone or spoken replies off (“stop listening”, “mute”). Anything else goes to the assistant.";

fn navi_slash(text: &str) -> Option<&'static str> {
    let head = text.split_whitespace().next()?.to_lowercase();
    Some(match head.as_str() {
        "/help" => "Use /help to see what Navi does without a model",
        "/sessions" => "Use /sessions to hear what each open harness is doing",
        "/progress" => "Use /progress to see how far along the active project is",
        "/assess" => "Use /assess to scan the project and ask a harness to estimate it",
        "/open" => "Use /open <harness>, for example /open claude code",
        "/mark" | "/reopen" => "Use /mark <task> done, in progress, blocked or todo",
        "/set" => "Use /set <part> to <percent>%, for example /set voice to 80%",
        _ => return None,
    })
}

/// A title reduced to lowercase words, so "Barge-in" and "barge in" match.
fn loose_title(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// "set the UI to 80 percent", "mark voice at 40%", "the UI is 70 percent
/// done", "put storage at 25%". Returns the part (`None` is the whole
/// project) and the percent.
fn parse_set_progress(lower: &str) -> Option<(Option<String>, f64)> {
    let words: Vec<&str> = lower.split_whitespace().collect();
    let (index, number) = words.iter().enumerate().find_map(|(index, word)| {
        let value: f64 = word.trim_end_matches('%').parse().ok()?;
        let marked = word.ends_with('%')
            || words
                .get(index + 1)
                .is_some_and(|next| next.starts_with("percent"));
        // "Put the station UI at 75": a bare number ends a set command.
        let bare = index + 1 == words.len()
            && index > 0
            && matches!(words[index - 1], "at" | "to")
            && matches!(words[0], "set" | "put" | "bump" | "move" | "raise" | "lower" | "mark");
        ((marked || bare) && (0.0..=100.0).contains(&value)).then_some((index, value))
    })?;
    let before = words[..index].join(" ");
    let name = if let Some(rest) = before.strip_prefix("set ") {
        rest.strip_suffix(" to")
            .or_else(|| rest.strip_suffix(" at"))?
    } else if let Some(rest) = ["mark ", "put ", "bump ", "move ", "raise ", "lower "]
        .iter()
        .find_map(|verb| before.strip_prefix(verb))
    {
        rest.strip_suffix(" at")
            .or_else(|| rest.strip_suffix(" as"))
            .or_else(|| rest.strip_suffix(" to"))
            .unwrap_or(rest)
    } else {
        before
            .strip_suffix(" is at")
            .or_else(|| before.strip_suffix(" is"))
            .or_else(|| before.strip_suffix(" are at"))
            .or_else(|| before.strip_suffix(" are"))?
    };
    let name = name.trim();
    let name = name.strip_prefix("the ").unwrap_or(name);
    let name = ["progress of ", "progress for ", "completion of "]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name);
    let name = ["'s progress", " progress", " completion"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    let name = name.strip_prefix("the ").unwrap_or(name).trim();
    if name.is_empty() || name.split_whitespace().count() > 5 {
        return None;
    }
    let whole = matches!(
        name,
        "project" | "whole project" | "this project" | "overall" | "everything" | "it" | "this"
            | "progress" | "completion" | "we" | "we're" | "i" | "us" | "all"
    );
    Some((if whole { None } else { Some(name.to_string()) }, number))
}

/// Live sessions' labels as the session strip shows them: the name, numbered
/// "#n" by creation when one project has several of that name.
/// The open sessions a spoken name picks: "Codex #2", "the terminal", "it".
fn named_sessions(sessions: &[Value], labels: &HashMap<String, String>, active: Option<&str>, name: &str) -> Vec<Value> {
    let name = spoken_numbers(name.trim().trim_end_matches([':', ',']));
    let name = name.strip_prefix("the ").unwrap_or(&name).to_string();
    sessions
        .iter()
        .filter(|session| session["running"] == true)
        .filter(|session| {
            let id = session["id"].as_str().unwrap_or_default();
            (matches!(name.as_str(), "this" | "it" | "active" | "this session" | "active session")
                && active == Some(id))
                || labels.get(id).is_some_and(|label| spoken_numbers(label) == name)
                || ["name", "profile", "harness"].iter().any(|key| {
                    session[*key].as_str().is_some_and(|value| spoken_numbers(value) == name)
                })
                || (session["profile"] == "claude" && name == "claude code")
        })
        .map(|session| session["id"].clone())
        .collect()
}

fn session_labels(sessions: &[Value]) -> HashMap<String, String> {
    let mut live: Vec<&Value> = sessions.iter().filter(|session| session["running"] == true).collect();
    live.sort_by_key(|session| session["created"].as_i64().unwrap_or(0));
    let key = |session: &Value| format!("{}\u{0}{}", session["project_id"], session["name"]);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for session in &live {
        *counts.entry(key(session)).or_default() += 1;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut labels = HashMap::new();
    for session in live {
        let name = session["name"].as_str().unwrap_or("session");
        let index = seen.entry(key(session)).or_default();
        *index += 1;
        let label = if counts[&key(session)] > 1 { format!("{name} #{index}") } else { name.to_string() };
        labels.insert(session["id"].as_str().unwrap_or_default().to_string(), label);
    }
    labels
}

/// "Which one: Codex in Navi, Codex in acme-api?": labels are numbered per
/// project, so a label shared across projects gains its project's name.
fn choice_labels(chosen: &[&Value], labels: &HashMap<String, String>, projects: &[Value]) -> Vec<String> {
    let names: Vec<String> = chosen
        .iter()
        .map(|session| {
            labels
                .get(session["id"].as_str().unwrap_or_default())
                .cloned()
                .unwrap_or_else(|| session["name"].as_str().unwrap_or("the session").to_string())
        })
        .collect();
    chosen
        .iter()
        .zip(&names)
        .map(|(session, name)| {
            if names.iter().filter(|other| *other == name).count() < 2 {
                return name.clone();
            }
            let project = projects
                .iter()
                .find(|project| project["id"] == session["project_id"])
                .and_then(|project| project["name"].as_str())
                .unwrap_or("no project");
            format!("{name} in {project}")
        })
        .collect()
}

/// The answer to a "Which one" question asked in the last minute ("the
/// second one", "acme-api", "Codex #2"): the pending action narrowed to it.
fn resolve_choice(text: &str, pending: &Value) -> Option<Value> {
    if now().saturating_sub(pending["time"].as_u64()?) > 60_000 {
        return None;
    }
    let choices = pending["choices"].as_array()?;
    let lower = conversational_text(text).to_lowercase();
    // A yes-or-no question: anything else is a new command.
    if pending["confirm"] == true {
        let answer = lower.trim().trim_end_matches(['.', '!']).trim();
        return match answer {
            "yes" | "yeah" | "yep" | "yes please" | "sure" | "do it" | "confirm" | "go ahead" | "okay" | "ok"
            | "yes do it" | "correct" => Some(pending["action"].clone()),
            "no" | "nope" | "no thanks" | "don't" | "cancel" | "never mind" | "stop" | "no don't" => {
                Some(json!({"kind":"ack","reply":"Okay, nothing was closed."}))
            }
            _ => None,
        };
    }
    let mut spoken = lower.trim().trim_end_matches(['.', '!', '?']).trim();
    for prefix in ["the one in ", "the one ", "the ", "number ", "in "] {
        spoken = spoken.strip_prefix(prefix).unwrap_or(spoken);
    }
    spoken = spoken.strip_suffix(" please").unwrap_or(spoken).trim();
    // "The second one", but "Codex number one" keeps its number.
    if let Some(head) = spoken.strip_suffix(" one")
        && matches!(head, "first" | "second" | "third" | "fourth" | "last")
    {
        spoken = head;
    }
    let spoken = spoken_numbers(spoken);
    let label = |choice: &Value| spoken_numbers(&choice["label"].as_str().unwrap_or_default().to_lowercase());
    let ordinal = match spoken.as_str() {
        "first" | "1" | "1st" => Some(0),
        "second" | "2" | "2nd" => Some(1),
        "third" | "3" | "3rd" => Some(2),
        "fourth" | "4" | "4th" => Some(3),
        "last" => choices.len().checked_sub(1),
        _ => None,
    };
    let only = |hits: Vec<usize>| (hits.len() == 1).then(|| hits[0]);
    let index = ordinal.filter(|index| *index < choices.len()).or_else(|| {
        only((0..choices.len()).filter(|index| label(&choices[*index]) == spoken).collect()).or_else(|| {
            (spoken.len() >= 3)
                .then(|| only((0..choices.len()).filter(|index| label(&choices[*index]).contains(&spoken)).collect()))
                .flatten()
        })
    })?;
    let choice = &choices[index];
    let mut action = pending["action"].clone();
    if action["kind"] == "switch" {
        action = json!({"kind":"switch","id":choice["id"],"name":choice["name"]});
    } else if action["kind"] == "task_state" {
        action["task"] = choice["task"].clone();
    } else {
        action["ids"] = json!([choice["id"]]);
    }
    Some(action)
}

/// Whether `object` (already through `spoken_numbers`) is this session's
/// name, profile or harness; "claude code" also names the claude profile.
fn names_session(session: &Value, object: &str) -> bool {
    ["name", "profile", "harness"]
        .iter()
        .any(|key| session[*key].as_str().is_some_and(|value| spoken_numbers(value) == object))
        || (session["profile"] == "claude" && object == "claude code")
}

/// Finished, not-yet-reopened sessions' labels as the Recent group shows
/// them: numbered "#n" newest first when one project has several of a name.
fn finished_labels(sessions: &[Value]) -> HashMap<String, String> {
    let mut finished: Vec<&Value> = sessions
        .iter()
        .filter(|session| session["running"] != true && session["replaced_by"].is_null())
        .collect();
    finished.sort_by_key(|session| std::cmp::Reverse(session["last_active"].as_u64().unwrap_or(0)));
    let key = |session: &Value| format!("{}\u{0}{}", session["project_id"], session["name"]);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for session in &finished {
        *counts.entry(key(session)).or_default() += 1;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut labels = HashMap::new();
    for session in finished {
        let name = session["name"].as_str().unwrap_or("session");
        let index = seen.entry(key(session)).or_default();
        *index += 1;
        let label = if counts[&key(session)] > 1 { format!("{name} #{index}") } else { name.to_string() };
        labels.insert(session["id"].as_str().unwrap_or_default().to_string(), label);
    }
    labels
}

/// Lowercase words with "#", "number" and spoken digits normalised, so
/// "terminal number two" and "Terminal #2" compare equal.
fn spoken_numbers(text: &str) -> String {
    text.to_lowercase()
        .replace('#', " ")
        .split_whitespace()
        .filter(|word| *word != "number")
        .map(|word| match word {
            "one" => "1",
            "two" => "2",
            "three" => "3",
            "four" => "4",
            "five" => "5",
            "six" => "6",
            "seven" => "7",
            "eight" => "8",
            "nine" => "9",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn literal_name(text: &str) -> Option<String> {
    for marker in [" called ", " named "] {
        if let Some(position) = text.to_lowercase().find(marker) {
            let name = text[position + marker.len()..]
                .trim()
                .trim_matches(['\"', '\'']);
            if !name.is_empty() {
                return Some(name.into());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_bound_to_words_and_deferred_commands_do_not_act() {
        assert!(!mentioned("Open a picture", "Pi"));
        assert!(!mentioned("Switch to Designer", "Design"));
        assert!(mentioned("Switch to Design.", "Design"));
        assert!(negated("Maybe later we can open Codex"));
        assert!(negated("What does switch to Review mean?"));
        assert!(!negated("Could you launch Claude Code?"));
        assert!(immediate_request("Could you launch Claude Code?"));
        assert!(!immediate_request("Can you explain how to open Codex?"));
        assert!(!immediate_request("Open Codex tomorrow"));
        assert!(immediate_request("Open Codex called Tomorrow"));
        let settings = json!({"projects":[{"name":"Navi","path":"/registered"}]});
        assert_eq!(
            project_for("Open Claude in Navi, please.", &settings).unwrap(),
            json!("/registered")
        );
        assert!(project_for("Open Codex in Atlantis", &settings).is_err());
    }
    #[test]
    fn persona_switch_phrases_are_parsed_deterministically() {
        assert_eq!(persona_target("Switch to navi mode"), Some("navi".into()));
        assert_eq!(persona_target("switch to navi."), Some("navi".into()));
        assert_eq!(persona_target("Be concise"), Some("concise".into()));
        assert_eq!(persona_target("be brief!"), Some("brief".into()));
        assert_eq!(persona_target("use navi persona"), Some("navi".into()));
        assert_eq!(persona_target("Open Codex"), None);
        assert_eq!(persona_target("be "), None);
        assert_eq!(
            persona_target("switch to the navi mode"),
            Some("the navi".into())
        );
        assert_eq!(persona_target("be very concise right now"), None);
    }
    #[test]
    fn non_tool_decisions_become_conversation() {
        // "what's the capital of France" lands on clarify today; it must reach
        // the assistant instead of a clarification prompt.
        assert!(routes_to_conversation("clarify"));
        assert!(routes_to_conversation("noop"));
        assert!(routes_to_conversation(""));
        for kind in [
            "launch",
            "switch",
            "browser",
            "url",
            "browser_action",
            "desktop_action",
        ] {
            assert!(!routes_to_conversation(kind), "{kind}");
        }
    }
    #[test]
    fn wake_phrases_are_stripped_before_conversation() {
        assert_eq!(
            conversational_text("hey navi, what's the capital of France"),
            "what's the capital of France"
        );
        assert_eq!(
            conversational_text("Navi what is the tallest mountain"),
            "what is the tallest mountain"
        );
        assert_eq!(
            conversational_text("  navi: tell me a one-line joke  "),
            "tell me a one-line joke"
        );
        assert_eq!(
            conversational_text("what is the tallest mountain"),
            "what is the tallest mountain"
        );
        assert_eq!(conversational_text("hey navi"), "hey navi");
        assert_eq!(conversational_text("navigation tips"), "navigation tips");
    }
    #[test]
    fn remote_and_malformed_endpoints_are_rejected() {
        for endpoint in [
            "https://example.com/v1/systemone",
            "http://127.0.0.1:0/v1/systemone",
            "http://127.0.0.1:80/v1/chat/completions",
            "http://127.0.0.1:80/v1/systemone?redirect=remote",
        ] {
            assert!(validate_endpoint(endpoint).is_err());
        }
        assert!(validate_endpoint("http://127.0.0.1:8461/v1/systemone").is_ok());
    }
    #[test]
    fn decision_cannot_execute_an_unknown_or_low_confidence_action() {
        let options = json!({"launch:codex":"Launch Codex","clarify":"Clarify"});
        assert!(
            answer(
                &json!({"action":{"value":"run_shell"}}),
                options.as_object().unwrap(),
                0.65
            )
            .is_err()
        );
        assert_eq!(
            answer(
                &json!({"action":{"value":"launch:codex","probability":0.4}}),
                options.as_object().unwrap(),
                0.65
            )
            .unwrap(),
            "clarify"
        );
    }
    #[test]
    fn gateway_lines_map_to_stream_events() {
        let mut turn = GatewayTurn::default();

        // A full verified-schema tool turn: status, one final, reasoning,
        // deltas that must concatenate into the reply.
        let mut event = map_gateway_line(r#"{"type":"delta","text":"Hello"}"#).unwrap();
        assert!(turn.record(&mut event));
        assert!(matches!(event, GatewayLine::Delta(ref text) if text == "Hello"));

        let mut event =
            map_gateway_line(r#"{"type":"reasoning","text":"thinking about it"}"#).unwrap();
        assert!(turn.record(&mut event));
        match event {
            GatewayLine::Reasoning(text, first) => {
                assert_eq!(text, "thinking about it");
                assert!(first, "first reasoning chunk marks the thinking state");
            }
            _ => panic!("reasoning line did not map"),
        }
        // A second reasoning line is content but no longer "first".
        let mut event = map_gateway_line(r#"{"type":"reasoning","text":"more"}"#).unwrap();
        assert!(turn.record(&mut event));
        assert!(matches!(event, GatewayLine::Reasoning(_, false)));

        let mut event = map_gateway_line(
            r#"{"type":"tool_status","id":"call_1","name":"shell","status":"running","input":{"command":"ls"}}"#,
        )
        .unwrap();
        assert!(turn.record(&mut event));
        match event {
            GatewayLine::ToolStatus { id, name, .. } => {
                assert_eq!(id, "call_1");
                assert_eq!(name.as_deref(), Some("shell"));
            }
            _ => panic!("tool_status line did not map"),
        }
        let final_line = r#"{"type":"tool","id":"call_1","name":"shell","status":"completed","input":{"command":"ls"},"output":"README.md","exit":0,"final":true}"#;
        let mut event = map_gateway_line(final_line).unwrap();
        assert!(turn.record(&mut event));
        match event {
            GatewayLine::ToolFinal { id, status, exit, .. } => {
                assert_eq!(id, "call_1");
                assert_eq!(status, "completed");
                assert_eq!(exit, json!(0));
            }
            _ => panic!("tool line did not map"),
        }
        // A duplicated final for the same id produces no second event.
        let mut event = map_gateway_line(final_line).unwrap();
        assert!(!turn.record(&mut event), "final rows are deduplicated by id");

        let mut event = map_gateway_line(
            r#"{"type":"permission","id":"perm_1","tool":"shell","input":{"command":"rm x"}}"#,
        )
        .unwrap();
        assert!(turn.record(&mut event));
        match event {
            GatewayLine::Permission { id, tool, .. } => {
                assert_eq!(id, json!("perm_1"));
                assert_eq!(tool.as_deref(), Some("shell"));
            }
            _ => panic!("permission line did not map"),
        }
        let mut event =
            map_gateway_line(r#"{"type":"warning","text":"directory ignored"}"#).unwrap();
        assert!(turn.record(&mut event));
        assert!(matches!(event, GatewayLine::Warning(ref text) if text == "directory ignored"));

        let mut event = map_gateway_line(r#"{"type":"delta","text":" world"}"#).unwrap();
        assert!(turn.record(&mut event));
        assert_eq!(turn.accumulated, "Hello world",
            "the reply text is the concatenated deltas");

        let mut event =
            map_gateway_line(r#"{"type":"done","text":"Hello world","session":"ses_1",
                "usage":{"tokens":{"input":10,"output":5,"reasoning":2},"cost":0.002},
                "session_usage":{"totalTokens":17},"finish":"stop",
                "actual_model":{"id":"m","providerID":"p"},"actual_agent":"navi"}"#)
            .unwrap();
        assert!(turn.record(&mut event));
        match event {
            GatewayLine::Done(done) => {
                assert_eq!(done["usage"]["cost"], json!(0.002));
                assert_eq!(done["session_usage"]["totalTokens"], json!(17));
                assert_eq!(done["actual_model"]["id"], json!("m"));
                assert_eq!(done["actual_agent"], json!("navi"));
            }
            _ => panic!("done line did not map"),
        }

        // Errors and unknown lines.
        assert!(matches!(
            map_gateway_line(r#"{"type":"error","text":"dead"}"#),
            Some(GatewayLine::Error(ref text)) if text == "dead"
        ));
        assert!(map_gateway_line("not json").is_none());
        assert!(map_gateway_line(r#"{"type":"step","x":1}"#).is_none());
    }
    #[test]
    fn attachment_params_are_bounded_and_checked() {
        assert!(validate_attachments(&Value::Null).is_ok());
        assert!(validate_attachments(&json!([{"name":"a.txt","data":"aGk="}])).is_ok());
        assert!(validate_attachments(&json!([{"name":"a\u{7}bad","data":"aGk="}])).is_err());
        assert!(validate_attachments(&json!([{"name":"a"}])).is_err());
        assert!(validate_attachments(&json!([{"path":"/tmp/x"}])).is_ok());
        let many = json!([{"path":"/a"},{"path":"/b"},{"path":"/c"},{"path":"/d"},{"path":"/e"}]);
        assert!(validate_attachments(&many).is_err());
        let big = "A".repeat(2 * 1024 * 1024 * 4 / 3 + 8);
        assert!(validate_attachments(&json!([{"data":big}])).is_err());
    }
    #[test]
    fn base64_decodes_and_context_is_capped() {
        assert_eq!(b64_decode("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(b64_decode("aGVsbG8"), None);
        assert_eq!(b64_decode("aGVsbG8!!!="), None);
        assert_eq!(b64_decode("aGk="), Some(b"hi".to_vec()));
        let long = "x".repeat(1000);
        assert_eq!(cap_block(&long).len(), 600);
        assert_eq!(cap_block("short"), "short");
    }
}
