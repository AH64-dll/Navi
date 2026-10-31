mod chief;
mod sessions;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Result<T> = std::result::Result<T, String>;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

pub struct State {
    db: Connection,
    settings: Value,
    events: VecDeque<Value>,
    next_event: u64,
    browser: Value,
    browser_seen: u64,
    browser_queue: VecDeque<Value>,
    browser_results: Value,
    desktop: Value,
    voice: Value,
    gui_seen: u64,
}

pub struct App {
    state: Mutex<State>,
    pub root: PathBuf,
    pub data: PathBuf,
    pub runtime: PathBuf,
    pub isolated: bool,
    chief_process: Mutex<Option<Child>>,
    command_lock: Mutex<()>,
    pub generation: AtomicU64,
    workers: Mutex<Vec<Child>>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn id() -> String {
    let mut bytes = [0_u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("OS randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn paths() -> (PathBuf, PathBuf) {
    let home = std::env::var("HOME").expect("HOME required");
    let data = PathBuf::from(
        std::env::var("NAVI_DATA_DIR").unwrap_or(format!("{home}/.local/share/navi")),
    );
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or(format!("/tmp/navi-{}", unsafe { libc::geteuid() }));
    let runtime =
        PathBuf::from(std::env::var("NAVI_RUNTIME_DIR").unwrap_or(format!("{base}/navi-station")));
    (data, runtime)
}

fn private_dir(path: &PathBuf) -> Result<()> {
    fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!("Untrusted directory: {}", path.display()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}

impl App {
    fn new() -> Result<Arc<Self>> {
        let isolated = std::env::var("NAVI_ISOLATED").as_deref() == Ok("1");
        if isolated
            && ["NAVI_DATA_DIR", "NAVI_RUNTIME_DIR"]
                .iter()
                .any(|key| std::env::var(key).is_err())
        {
            return Err(
                "Isolated mode requires explicit private data and runtime directories".into(),
            );
        }
        let (data, runtime) = paths();
        private_dir(&data)?;
        private_dir(&runtime)?;
        let db = Connection::open(data.join("station.sqlite3")).map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, name TEXT NOT NULL, profile TEXT NOT NULL, cwd TEXT NOT NULL, created INTEGER NOT NULL);").map_err(|e| e.to_string())?;
        let default = json!({
            "model": {"name":"d1-3B Q4", "endpoint":"http://127.0.0.1:8461/v1/systemone", "minimum_probability":0.65,
                "server": data.join("runtime/llama-b11540/llama-b11540/llama-server"),
                "weights":data.join("models/d1-3B/d1-3B-Q4_K_M.gguf"),
                "projection":data.join("models/d1-3B/mmproj-d1-3B-Q8_0.gguf"), "gpu_layers":99, "threads":2},
            "speech":{"engine":"whistle", "weights":data.join("models/whistle/whistle.cact"), "wake_score":1.0, "wake_threshold":0.25},
            "microphone":false, "listening":"background", "active_session":null, "persona":"jarvis",
            "profiles":[{"id":"codex","name":"Codex","executable":"codex","args":[]},
                {"id":"claude","name":"Claude Code","executable":"claude","args":[]},
                {"id":"pi","name":"Pi","executable":"pi","args":[]},
                {"id":"terminal","name":"Terminal","executable":"/bin/bash","args":[]}],
            "projects":[{"name":"Navi","path":PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).ok_or("Missing project root")?}],
            "favorites":[], "shortcuts":{"focus":"F10","fullscreen":"F11"}
        });
        let settings = db
            .query_row("SELECT value FROM settings WHERE id=1", [], |row| {
                row.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(default);
        let root = PathBuf::from(
            std::env::var("NAVI_APP_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").into()),
        );
        let app = Arc::new(Self {
            state: Mutex::new(State {
                db,
                settings,
                events: VecDeque::new(),
                next_event: 0,
                browser: json!({}),
                browser_seen: 0,
                browser_queue: VecDeque::new(),
                browser_results: json!({}),
                desktop: json!({"status":"disconnected"}),
                voice: json!({"status":"off"}),
                gui_seen: 0,
            }),
            root,
            data,
            runtime,
            isolated,
            chief_process: Mutex::new(None),
            command_lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            workers: Mutex::new(Vec::new()),
        });
        app.event("ready", "Rust orchestration service ready", json!({}));
        Ok(app)
    }

    pub fn event(&self, kind: &str, message: &str, details: Value) {
        let mut state = self.state.lock().unwrap();
        state.next_event += 1;
        let seq = state.next_event;
        state.events.push_back(
            json!({"seq":seq,"time":now(),"kind":kind,"message":message,"details":details}),
        );
        while state.events.len() > 100 {
            state.events.pop_front();
        }
    }

    pub fn settings(&self) -> Value {
        self.state.lock().unwrap().settings.clone()
    }

    fn save_settings(&self, settings: Value) -> Result<Value> {
        if settings["microphone"].as_bool().is_none() {
            return Err("Microphone setting must be true or false".into());
        }
        if self.isolated && settings["microphone"] == true {
            return Err("Microphone capture is disabled in isolated mode".into());
        }
        let model = &settings["model"];
        chief::validate_endpoint(
            model["endpoint"]
                .as_str()
                .ok_or("Missing local model endpoint")?,
        )?;
        if !matches!(settings["listening"].as_str(), Some("background" | "gui")) {
            return Err("Listening mode must be background or gui".into());
        }
        if !matches!(
            settings["speech"]["engine"].as_str(),
            None | Some("whistle" | "moonshine" | "whisper")
        ) {
            return Err("Speech engine must be whistle, moonshine, or whisper".into());
        }
        if model.get("threads").is_some()
            && !model["threads"]
                .as_u64()
                .is_some_and(|value| (1..=8).contains(&value))
        {
            return Err("Model threads must be between 1 and 8".into());
        }
        let probability = model["minimum_probability"]
            .as_f64()
            .ok_or("Missing model confidence threshold")?;
        if !(0.5..=1.0).contains(&probability) {
            return Err("Confidence must be between 0.5 and 1".into());
        }
        for field in ["profiles", "projects", "favorites"] {
            let entries = settings[field]
                .as_array()
                .ok_or(format!("Missing {field}"))?;
            if entries.len() > 100 {
                return Err(format!("Too many {field}"));
            }
        }
        for profile in settings["profiles"].as_array().unwrap() {
            let executable = profile["executable"].as_str().ok_or("Missing executable")?;
            if executable.is_empty() || executable.starts_with('-') {
                return Err("Invalid executable".into());
            }
            if !profile["args"]
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string))
            {
                return Err("Launch arguments must be an array of strings".into());
            }
        }
        let old = self.settings();
        if ["model", "profiles", "projects", "favorites"]
            .iter()
            .any(|key| old[key] != settings[key])
        {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.state.lock().unwrap().browser_queue.clear();
        }
        let restart_model = {
            [
                "server",
                "weights",
                "projection",
                "endpoint",
                "gpu_layers",
                "threads",
            ]
            .iter()
            .any(|key| old["model"][key] != model[key])
        };
        if restart_model && let Some(mut child) = self.chief_process.lock().unwrap().take() {
            child.kill().map_err(|error| error.to_string())?;
            child.wait().map_err(|error| error.to_string())?;
        }
        let mut state = self.state.lock().unwrap();
        state
            .db
            .execute(
                "INSERT OR REPLACE INTO settings VALUES(1,?1)",
                [settings.to_string()],
            )
            .map_err(|e| e.to_string())?;
        state.settings = settings;
        Ok(state.settings.clone())
    }

    pub fn set_active(&self, session: &str) -> Result<()> {
        let mut settings = self.settings();
        settings["active_session"] = json!(session);
        self.save_settings(settings)?;
        self.event("session", "Session selected", json!({"id":session}));
        Ok(())
    }

    pub fn browser_action(&self, action: Value, generation: u64) -> Result<Value> {
        let ticket = id();
        {
            let mut s = self.state.lock().unwrap();
            if now().saturating_sub(s.browser_seen) > 3000 {
                return Err("Connect the Zen extension first".into());
            }
            s.browser_queue
                .push_back(json!({"ticket":ticket,"expires":now()+5000,"action":action}));
        }
        let deadline = now() + 5500;
        while now() < deadline && self.generation.load(Ordering::SeqCst) == generation {
            {
                let mut s = self.state.lock().unwrap();
                if let Some(reply) = s.browser_results.as_object_mut().unwrap().remove(&ticket) {
                    return Ok(reply);
                }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        self.state
            .lock()
            .unwrap()
            .browser_queue
            .retain(|item| item["ticket"] != ticket);
        Err("Browser result unknown; the action was not retried".into())
    }

    pub fn python(&self) -> PathBuf {
        let venv = self.data.join("venv/bin/python");
        if venv.exists() {
            venv
        } else {
            PathBuf::from("python3")
        }
    }

    fn spawn_worker(&self, worker: &str) -> Result<Value> {
        if self.isolated {
            return Err("Live desktop and microphone workers are disabled in isolated mode".into());
        }
        let mut workers = self.workers.lock().unwrap();
        workers.retain_mut(|child| child.try_wait().ok().flatten().is_none());
        let pid_file = self.runtime.join(format!("{worker}.pid"));
        if let Ok(pid) = fs::read_to_string(&pid_file)
            .and_then(|s| s.parse::<i32>().map_err(std::io::Error::other))
            && unsafe { libc::kill(pid, 0) } == 0
        {
            return Ok(json!({"running":true,"pid":pid}));
        }
        let child = Command::new(if worker == "desktop" {
            PathBuf::from("/usr/bin/python3")
        } else {
            self.python()
        })
        .arg(self.root.join(format!("workers/{worker}.py")))
        .env("NAVI_RUNTIME_DIR", &self.runtime)
        .env("NAVI_DATA_DIR", &self.data)
        .env("NEEDLE_TELEMETRY", "0")
        .env("DO_NOT_TRACK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| e.to_string())?;
        fs::write(pid_file, child.id().to_string()).map_err(|e| e.to_string())?;
        let pid = child.id();
        workers.push(child);
        Ok(json!({"running":true,"pid":pid}))
    }

    fn dispatch(&self, request: Value) -> Result<Value> {
        let p = &request["params"];
        match request["op"].as_str().ok_or("Missing operation")? {
            "ping" => Ok(json!({"version":"0.1.0","pid":std::process::id()})),
            "status" => {
                let sessions = self.sessions()?;
                let mut s = self.state.lock().unwrap();
                if p["gui"].as_bool() == Some(true) {
                    s.gui_seen = now();
                }
                let since = p["since"].as_u64().unwrap_or(0);
                Ok(json!({"settings":s.settings,"sessions":sessions,
                    "isolated":self.isolated,
                    "generation":self.generation.load(Ordering::SeqCst),
                    "events":s.events.iter().filter(|e| e["seq"].as_u64().unwrap_or(0)>since).collect::<Vec<_>>(),
                    "browser":s.browser,"browser_connected":now().saturating_sub(s.browser_seen)<3000,
                    "desktop":s.desktop,"voice":s.voice, "gui_open":now().saturating_sub(s.gui_seen)<2000}))
            }
            "settings" => self.save_settings(p.clone()),
            "create" => self.create_session(p),
            "browser_popup" => self.browser_action(
                json!({"type":"popup"}),
                self.generation.load(Ordering::SeqCst),
            ),
            "attach_tab" => {
                let tab = p["id"].as_i64().ok_or("Missing browser tab ID")?;
                let allowed = self.state.lock().unwrap().browser["tabs"]
                    .as_array()
                    .is_some_and(|tabs| {
                        tabs.iter().any(|item| {
                            item["id"] == tab
                                && item["url"].as_str().is_some_and(|url| {
                                    url.starts_with("http://") || url.starts_with("https://")
                                })
                        })
                    });
                if !allowed {
                    return Err("Select a currently registered web tab".into());
                }
                self.browser_action(
                    json!({"type":"attach","tab":tab}),
                    self.generation.load(Ordering::SeqCst),
                )
            }
            "select" => {
                let target = p["id"].as_str().ok_or("Missing session")?;
                self.require_session(target)?;
                self.set_active(target)?;
                Ok(json!({"id":target}))
            }
            "command" | "decide" => {
                let text = p["text"].as_str().ok_or("Missing command")?;
                if text.is_empty() || text.len() > 2000 {
                    return Err("Command must contain 1 to 2000 bytes".into());
                }
                if request["op"] == "command"
                    && let Some(persona) = self.persona_switch(text)?
                {
                    return Ok(
                        json!({"trace":[],"dry_run":false,"reply":persona.reply,"persona":persona.name}),
                    );
                }
                let _lock = self
                    .command_lock
                    .try_lock()
                    .map_err(|_| "Chief is busy; stop the current command first")?;
                let generation = self.generation.load(Ordering::SeqCst);
                self.run_command(text, request["op"] == "decide", generation)
            }
            "cancel" => {
                self.generation.fetch_add(1, Ordering::SeqCst);
                self.state.lock().unwrap().browser_queue.clear();
                self.event("stopped", "Current command stopped", json!({}));
                Ok(json!({"stopped":true}))
            }
            "voice_start" => self.spawn_worker("voice"),
            "desktop_start" => self.spawn_worker("desktop"),
            "desktop_stop" => {
                let pid_file = self.runtime.join("desktop.pid");
                if let Ok(pid) = fs::read_to_string(&pid_file)
                    .and_then(|value| value.parse::<u32>().map_err(std::io::Error::other))
                {
                    let mut workers = self.workers.lock().unwrap();
                    if let Some(index) = workers.iter().position(|child| child.id() == pid) {
                        let mut child = workers.remove(index);
                        let _ = child.kill();
                        child.wait().map_err(|error| error.to_string())?;
                    }
                }
                if pid_file.exists() {
                    fs::remove_file(pid_file).map_err(|error| error.to_string())?;
                }
                self.state.lock().unwrap().desktop = json!({"status":"disconnected"});
                Ok(json!({"stopped":true}))
            }
            "voice_state" => {
                self.state.lock().unwrap().voice = p.clone();
                Ok(json!({}))
            }
            "desktop_state" => {
                self.state.lock().unwrap().desktop = p.clone();
                Ok(json!({}))
            }
            "browser_sync" => {
                let mut s = self.state.lock().unwrap();
                s.browser_seen = now();
                if let Some(snapshot) = p.get("snapshot") {
                    s.browser = snapshot.clone();
                }
                if let Some(reply) = p.get("result")
                    && let Some(ticket) = reply["ticket"].as_str()
                {
                    s.browser_results[ticket] = reply.clone();
                    if s.browser_results.as_object().unwrap().len() > 32 {
                        s.browser_results = json!({});
                    }
                }
                while s
                    .browser_queue
                    .front()
                    .is_some_and(|a| a["expires"].as_u64().unwrap_or(0) < now())
                {
                    s.browser_queue.pop_front();
                }
                Ok(s.browser_queue.pop_front().unwrap_or(json!(null)))
            }
            "desktop_action" => Err("Desktop actions are internal to the chief".into()),
            other => Err(format!("Unknown operation {other}")),
        }
    }
}

fn response(app: &App, request: Value) -> Value {
    match app.dispatch(request) {
        Ok(value) => json!({"ok":true,"result":value}),
        Err(error) => {
            app.event("error", &error, json!({}));
            json!({"ok":false,"error":error})
        }
    }
}

fn serve() -> Result<()> {
    let app = App::new()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(app.runtime.join("daemon.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Navi daemon already running".into());
    }
    let socket = app.runtime.join("station.sock");
    if socket.exists() {
        fs::remove_file(&socket).map_err(|e| e.to_string())?;
    }
    let listener = UnixListener::bind(socket).map_err(|e| e.to_string())?;
    if !app.isolated {
        app.spawn_worker("voice")?;
    }
    for connection in listener.incoming() {
        let app = Arc::clone(&app);
        if let Ok(mut stream) = connection {
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut reader = BufReader::new(&mut stream).take((MAX_MESSAGE + 1) as u64);
                let mut line = String::new();
                let reply = match reader.read_line(&mut line) {
                    Ok(_) if line.len() <= MAX_MESSAGE => serde_json::from_str(&line)
                        .map(|r| response(&app, r))
                        .unwrap_or(json!({"ok":false,"error":"Invalid JSON"})),
                    _ => json!({"ok":false,"error":"Request too large or timed out"}),
                };
                let _ = writeln!(stream, "{reply}");
            });
        }
    }
    Ok(())
}

fn rpc(request: Value) -> Result<Value> {
    let (_, runtime) = paths();
    let mut socket =
        UnixStream::connect(runtime.join("station.sock")).map_err(|e| e.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(90)))
        .map_err(|e| e.to_string())?;
    writeln!(socket, "{request}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(socket)
        .take(MAX_MESSAGE as u64)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

fn native() -> Result<()> {
    // Browser native messaging is little endian on supported Linux x86_64.
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut header = [0; 4];
        match input.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
        let size = u32::from_le_bytes(header) as usize;
        if size > MAX_MESSAGE {
            return Err("Native message too large".into());
        }
        let mut payload = vec![0; size];
        input.read_exact(&mut payload).map_err(|e| e.to_string())?;
        let request: Value = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
        if request["op"] != "browser_sync" {
            return Err("Native host only accepts browser synchronization".into());
        }
        let reply = rpc(request)
            .unwrap_or_else(|e| json!({"ok":false,"error":e}))
            .to_string();
        output
            .write_all(&(reply.len() as u32).to_le_bytes())
            .map_err(|e| e.to_string())?;
        output
            .write_all(reply.as_bytes())
            .map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
    }
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let result = match args.get(1).map(String::as_str) {
        Some("serve") => serve(),
        Some("native") => native(),
        Some("rpc") => serde_json::from_str(
            args.get(2)
                .map(String::as_str)
                .unwrap_or("{\"op\":\"status\"}"),
        )
        .map_err(|e| e.to_string())
        .and_then(rpc)
        .map(|value| println!("{value}")),
        _ => {
            println!("navid serve | native | rpc '<JSON request>'");
            Ok(())
        }
    };
    if let Err(error) = result {
        eprintln!("Navi: {error}");
        std::process::exit(1);
    }
}
