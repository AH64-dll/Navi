use crate::{App, Result, now};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

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
        if process
            .as_mut()
            .is_some_and(|p| p.try_wait().ok().flatten().is_none())
        {
            return Err("Local chief is still starting; try again shortly".into());
        }
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
        for _ in 0..120 {
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
        self.ensure_chief(generation)?;
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
            "focus",
            "existing",
        ]
        .iter()
        .any(|word| mentioned(text, word));
        let launching = ["open", "launch", "start", "create", "new", "fresh"]
            .iter()
            .any(|word| mentioned(text, word));
        let valid_project = project_for(text, &settings).is_ok();
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
    // "switch to jarvis mode", "be concise", "use friendly persona". Returns
    // the switch outcome so dispatch can answer without a decision round-trip.
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
        settings["persona"] = json!(name);
        self.save_settings(settings)?;
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

    // Send a conversational utterance to the OpenCode bridge and return the
    // assistant's spoken reply. Falls back to a fixed line when the bridge is
    // unreachable so the voice loop stays responsive.
    fn converse(&self, text: &str, generation: u64) -> Value {
        let settings = self.settings();
        let persona = settings["persona"].as_str().unwrap_or("jarvis");
        let mut child = match Command::new(self.python())
            .arg(self.root.join("workers/opencode_bridge.py"))
            .arg("--persona")
            .arg(persona)
            .arg(text)
            .env("NAVI_RUNTIME_DIR", &self.runtime)
            .env("NAVI_DATA_DIR", &self.data)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return json!({"text":format!("I could not reach the assistant: {error}")});
            }
        };
        let deadline = now() + 90_000;
        loop {
            if self.generation.load(Ordering::SeqCst) != generation {
                let _ = child.kill();
                let _ = child.wait();
                return json!({"text":"Conversation stopped."});
            }
            if let Ok(Some(status)) = child.try_wait() {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut output);
                }
                let output = output.trim().to_string();
                return if status.success() && !output.is_empty() {
                    json!({"text":output,"persona":persona,"source":"opencode"})
                } else {
                    json!({"text":"I could not reach the assistant. Is opencode serve running?","persona":persona,"source":"unavailable"})
                };
            }
            if now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return json!({"text":"The assistant took too long to answer.","persona":persona,"source":"timeout"});
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn run_command(&self, text: &str, dry: bool, generation: u64) -> Result<Value> {
        let clauses = text.split(" then ").collect::<Vec<_>>();
        if clauses.len() > 4 {
            return Err("Use at most four explicit steps in one command".into());
        }
        let mut trace = Vec::new();
        let mut reply = Value::Null;
        for clause in clauses {
            self.event("thinking", "Chief is deciding", json!({"text":clause}));
            let decision = self.decide(clause, generation)?;
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
            // asking the user to rephrase. Actions keep the same dispatch.
            let kind = action["kind"].as_str().unwrap_or("");
            let result = if routes_to_conversation(kind) {
                let reply = self.converse(&conversational_text(clause), generation);
                self.event(
                    "reply",
                    reply["text"].as_str().unwrap_or("No reply"),
                    json!({"persona":reply["persona"],"source":reply["source"],"text":reply["text"]}),
                );
                json!({"message":"No action requested","reply":reply})
            } else {
                match kind {
                    "launch" => {
                        let settings = self.settings();
                        let cwd = project_for(clause, &settings)?;
                        let name = literal_name(clause);
                        self.create_session(
                            &json!({"profile":action["profile"],"cwd":cwd,"name":name}),
                        )?
                    }
                    "switch" => {
                        let target = action["id"].as_str().ok_or("Missing session")?;
                        self.require_session(target)?;
                        self.set_active(target)?;
                        json!({"id":target})
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
                reply = r.clone();
            }
            self.event(
                "command",
                "Command completed",
                json!({"decision":decision,"result":result}),
            );
            trace.push(json!({"decision":decision,"result":result}));
        }
        Ok(json!({"trace":trace,"dry_run":dry,"reply":reply}))
    }
}

// Model action kinds that actually execute a tool. Everything else (noop,
// clarify, low confidence, an unexpected kind) runs no tool and is answered as
// conversation by the OpenCode bridge instead of a clarification prompt.
fn routes_to_conversation(kind: &str) -> bool {
    !matches!(
        kind,
        "launch" | "switch" | "browser" | "url" | "browser_action" | "desktop_action"
    )
}

// Strip a leading wake phrase before an utterance goes to the conversation
// bridge: "hey navi, what is X" and "navi what is X" both become "what is X".
// A bare wake word keeps the original text so the bridge still gets a request.
fn conversational_text(text: &str) -> String {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    for wake in ["hey navi", "okay navi", "ok navi", "navi"] {
        let Some(rest) = lower.strip_prefix(wake) else {
            continue;
        };
        // Require a word boundary so "navigation tips" keeps its text.
        if !rest.is_empty()
            && !rest.starts_with(|c: char| c.is_whitespace() || matches!(c, ',' | ':'))
        {
            continue;
        }
        let rest = &trimmed[trimmed.len() - rest.len()..];
        let stripped = rest
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ':'))
            .trim();
        return if stripped.is_empty() {
            trimmed.to_string()
        } else {
            stripped.to_string()
        };
    }
    trimmed.to_string()
}

// Deterministic voice-command grammar for persona changes, independent of the
// decision model: "switch to jarvis mode", "be concise", "use friendly
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
        assert_eq!(
            persona_target("Switch to jarvis mode"),
            Some("jarvis".into())
        );
        assert_eq!(persona_target("switch to jarvis."), Some("jarvis".into()));
        assert_eq!(persona_target("Be concise"), Some("concise".into()));
        assert_eq!(persona_target("be brief!"), Some("brief".into()));
        assert_eq!(
            persona_target("use friendly persona"),
            Some("friendly".into())
        );
        assert_eq!(persona_target("Open Codex"), None);
        assert_eq!(persona_target("be "), None);
        assert_eq!(
            persona_target("switch to the jarvis mode"),
            Some("the jarvis".into())
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
}
