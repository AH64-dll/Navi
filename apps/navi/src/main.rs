mod adapters;
mod chief;
mod detector;
mod link;
mod sessions;
mod store;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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

pub type Result<T, E = String> = std::result::Result<T, E>;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// Diagnostic window for the RAM tier of the event feed (BUG-098): one poll's
/// worth of history, replaced by a coalesced producer so it cannot overflow.
const EVENT_LIMIT: usize = 100;
/// Terminal events never leave memory: the answer the user is waiting for must
/// survive a burst of deltas that evicts the diagnostic ring.
const TERMINAL_LIMIT: usize = 16;
/// One status page. A truncated page sets `has_more` and the client drains it
/// immediately instead of waiting for the next poll (BUG-098/099).
const EVENT_PAGE: usize = 500;

pub const DEFAULT_NAVI_PROMPT: &str = "You are Navi, the user's coding agent on their own machine. Your name is Navi; that is your only name. Begin every reply with one plain spoken sentence that says what you did or what you need — the voice system reads only that line aloud. After it, answer with readable markdown: fenced code blocks, diffs, and lists are all welcome. Work inside the project directory you are given and never touch files outside it. Use the tools OpenCode provides for real work: read, edit, run, verify. Navi tracks how complete each project is. When you are asked how far along the project is, or you finish work that changes it, inspect the code and give your honest estimate, then end the reply with one line per part and one overall line in exactly this form: PROGRESS part \"<part>\" <percent> | <what you checked> and PROGRESS overall <percent> | <what you checked>. Navi records those lines. Base every number on what you read or ran; untested or stubbed work is not done.";
const LEGACY_NAVI_PROMPTS: [&str; 3] = [
    "You are Navi, the user's coding agent on their own machine. Your name is Navi; that is your only name. Begin every reply with one plain spoken sentence that says what you did or what you need — the voice system reads only that line aloud. After it, answer with readable markdown: fenced code blocks, diffs, and lists are all welcome. Work inside the project directory you are given and never touch files outside it. Use the tools OpenCode provides for real work: read, edit, run, verify. Never state a completion percentage and never claim progress you have not verified.",
    "You are Navi, the user's voice assistant on their own machine. Your name is Navi; that is your only name. Answer directly and concisely — one to three short sentences unless the user asks for detail. Plain prose: no markdown, no code blocks, no emoji, because your replies may be spoken aloud. You run inside the Navi desktop app and OpenCode provides your tools; use them when a request needs real work on the machine.",
    "You are Navi, the user's voice assistant on their own machine. Your name is Navi; you are not Jarvis and have no other persona name. Answer directly and concisely — one to three short sentences unless the user asks for detail. Plain prose: no markdown, no code blocks, no emoji, because your replies may be spoken aloud. You run inside the Navi desktop app and OpenCode provides your tools; use them when a request needs real work on the machine.",
];
pub const DEFAULT_ASSISTANT_MODEL: &str = "commandcode/deepseek/deepseek-v4.1-flash";
pub const DEFAULT_ASSISTANT_AGENT: &str = "navi";
pub const AGENT_FILE: &str = "navi.md";

pub struct State {
    db: Connection,
    boot_id: String,
    settings: Value,
    events: VecDeque<Value>,
    /// Terminal events (`reply`, `error`) kept outside the diagnostic ring.
    terminal_events: VecDeque<Value>,
    /// Authoritative view of the open assistant turn: `{turn,text,done}`.
    turn_state: Value,
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
    /// Weak self-reference so request handlers can start background work.
    me: std::sync::Weak<App>,
    state: Mutex<State>,
    pub root: PathBuf,
    pub data: PathBuf,
    pub runtime: PathBuf,
    pub isolated: bool,
    chief_process: Mutex<Option<Child>>,
    command_lock: Mutex<()>,
    command_queue: Mutex<VecDeque<Value>>,
    assistant_dir: Mutex<Option<String>>,
    pub generation: AtomicU64,
    pub config_version: AtomicU64,
    workers: Mutex<Vec<(String, Child)>>,
    worker_locks: Mutex<Vec<(String, std::fs::File)>>,
    desired_workers: Mutex<HashSet<String>>,
    worker_started: Mutex<HashMap<String, u64>>,
    worker_attempts: Mutex<HashMap<String, (u32, u64)>>,
    launch_path_cache: Mutex<Option<(u64, Vec<String>)>>,
    base_env_cache: Mutex<Option<(u64, BTreeMap<String, String>)>>,
    harness_cache: Mutex<Option<(u64, Value)>>,
    external_cache: Mutex<Option<(u64, Vec<Value>)>>,
    harness_seen: Mutex<BTreeSet<String>>,
    state_marks: Mutex<HashMap<(String, String), u64>>,
    /// Shared tmux truth for every status caller (BUG-094).
    status_cache: sessions::StatusCache,
    /// Post-prompt tail sampling schedule (BUG-087); one plan at a time.
    tail_plan: Mutex<Option<sessions::TailPlan>>,
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

thread_local! {
    /// The command this thread is running. Every event it emits carries the
    /// id as `request_id`, so a client keys one chat row per command whether
    /// the RPC reply, the live feed, or a replay after a restart arrives first.
    static REQUEST: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Restores the previous request when a command scope ends, even by panic.
struct RequestScope(Option<String>);

impl Drop for RequestScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        REQUEST.with(|cell| *cell.borrow_mut() = previous);
    }
}

/// Run `body` as request `request`: events emitted on this thread meanwhile
/// carry its id. `None` runs it untagged.
pub fn with_request<T>(request: Option<&str>, body: impl FnOnce() -> T) -> T {
    let previous = REQUEST.with(|cell| cell.replace(request.map(str::to_string)));
    let _scope = RequestScope(previous);
    body()
}

/// A client may name its own request id (it keys the row it already drew);
/// anything else gets a fresh one.
fn command_request_id(value: &Value) -> String {
    value
        .as_str()
        .filter(|text| {
            (1..=64).contains(&text.len())
                && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .map(str::to_string)
        .unwrap_or_else(|| format!("req-{}", id()))
}

/// Events that end a turn or report a failure. They are retained apart from the
/// diagnostic ring, so a flood of deltas can never evict the answer (BUG-098).
fn terminal_kind(kind: &str) -> bool {
    matches!(kind, "reply" | "error")
}

/// Content-addressed section version for `status`: a client that already holds
/// this content is not sent it again (BUG-094).
fn fingerprint(value: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.to_string().hash(&mut hasher);
    hasher.finish()
}

/// Record one optional `status` section. With `?sections=` the caller opts into
/// omission: an unchanged version is skipped instead of re-sent.
fn section(
    payload: &mut serde_json::Map<String, Value>,
    versions: &mut serde_json::Map<String, Value>,
    name: &str,
    value: Value,
    known: Option<u64>,
    selective: bool,
) {
    let version = fingerprint(&value);
    versions.insert(name.to_string(), json!(version));
    if !selective || known != Some(version) {
        payload.insert(name.to_string(), value);
    }
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
        // The server must keep dead panes so exit status and the final screen
        // survive a harness that exits immediately (BUG-073). A server started
        // without this config would drop both.
        let config = runtime.join("tmux.conf");
        let _ = fs::write(
            &config,
            "set -g remain-on-exit on\nset -g exit-empty off\nset -g status off\nset -g mouse on\n",
        );
        let _ = fs::set_permissions(&config, fs::Permissions::from_mode(0o600));
        // Start the private server once with no inherited pipes. With
        // exit-empty off the server outlives this client; if it inherited a
        // captured stdout, every later Command::output() would wait forever.
        let mut server = Command::new("tmux");
        server
            .arg("-f")
            .arg(&config)
            .arg("-S")
            .arg(data.join("terminals.sock"));
        if std::env::var("NAVI_MANAGED_TERMINALS").as_deref() == Ok("1") {
            server.arg("-N");
        }
        let _ = server
            .arg("start-server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let db = Connection::open(data.join("station.sqlite3")).map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, name TEXT NOT NULL, profile TEXT NOT NULL, cwd TEXT NOT NULL, created INTEGER NOT NULL);").map_err(|e| e.to_string())?;
        let default = json!({
            "model": {"name":"d1-3B Q4", "endpoint":"http://127.0.0.1:8461/v1/systemone", "minimum_probability":0.65,
                "server": data.join("runtime/llama-b11540/llama-b11540/llama-server"),
                "weights":data.join("models/d1-3B/d1-3B-Q4_K_M.gguf"),
                "projection":data.join("models/d1-3B/mmproj-d1-3B-Q8_0.gguf"), "gpu_layers":99, "threads":2},
            "speech":{"engine":"whistle", "weights":data.join("models/whistle/whistle.cact"), "wake_score":1.0, "wake_threshold":0.25},
            "microphone":false, "listening":"background", "active_session":null, "persona":"navi",
            "speak":false, "system_prompt":DEFAULT_NAVI_PROMPT,
            "assistant":{"model":DEFAULT_ASSISTANT_MODEL,"agent":DEFAULT_ASSISTANT_AGENT},
            "profiles":[{"id":"codex","name":"Codex","executable":"codex","args":[]},
                {"id":"claude","name":"Claude Code","executable":"claude","args":[]},
                {"id":"pi","name":"Pi","executable":"pi","args":[]},
                {"id":"opencode","name":"OpenCode","executable":"opencode","args":[]},
                {"id":"terminal","name":"Terminal","executable":"/bin/bash","args":[]}],
            // The source tree is a handy first project on the machine that
            // built Navi; elsewhere that path does not exist, and a first run
            // should not start with a "folder missing" project.
            "projects":PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2)
                .filter(|root| root.is_dir())
                .map(|root| vec![json!({"name":"Navi","path":root})])
                .unwrap_or_default(),
            "favorites":[], "shortcuts":{"focus":"F10","fullscreen":"F11"}
        });
        let mut settings = db
            .query_row("SELECT value FROM settings WHERE id=1", [], |row| {
                row.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(default);
        let before = settings.clone();
        if settings.get("speak").and_then(Value::as_bool).is_none() {
            settings["speak"] = json!(false);
        }
        if settings["persona"] == "jarvis" || settings["persona"].is_null() {
            settings["persona"] = json!("navi");
        }
        if settings
            .get("system_prompt")
            .and_then(Value::as_str)
            .is_none()
        {
            settings["system_prompt"] = json!(DEFAULT_NAVI_PROMPT);
        }
        if LEGACY_NAVI_PROMPTS
            .iter()
            .any(|prompt| settings["system_prompt"] == *prompt)
        {
            settings["system_prompt"] = json!(DEFAULT_NAVI_PROMPT);
        }
        if settings
            .get("assistant")
            .and_then(Value::as_object)
            .is_none()
        {
            settings["assistant"] =
                json!({"model":DEFAULT_ASSISTANT_MODEL,"agent":DEFAULT_ASSISTANT_AGENT});
        }
        // Profile v2 normalization on load (BUG-070/077/078): v1 profiles get
        // the new fields with safe defaults before the doc is compared.
        if let Some(profiles) = settings["profiles"].as_array_mut() {
            for profile in profiles.iter_mut() {
                if profile["harness"].is_null() {
                    profile["harness"] = json!(std::path::Path::new(
                        profile["executable"].as_str().unwrap_or("")
                    )
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("shell"));
                }
                if profile["args"].is_null() {
                    profile["args"] = json!([]);
                }
                if profile["env"].is_null() {
                    profile["env"] = json!({});
                }
                if profile["cwd"].is_null() {
                    profile["cwd"] = json!("inherit");
                }
                if profile["resume_default"].is_null() {
                    profile["resume_default"] = json!("ask");
                }
                if profile["interrupt_keys"].is_null() {
                    profile["interrupt_keys"] = json!([]);
                }
            }
            settings["profiles_rev"] = json!(2);
        }
        if settings != before {
            db.execute(
                "INSERT OR REPLACE INTO settings VALUES(1,?1)",
                [settings.to_string()],
            )
            .map_err(|e| e.to_string())?;
        }
        store::migrate(&db, &settings)?;
        let boot_id = id();
        let persisted_seq = db
            .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0) as u64;
        let next_event = persisted_seq.max(now() * 1024);
        let root = PathBuf::from(
            std::env::var("NAVI_APP_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").into()),
        );
        let app = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            state: Mutex::new(State {
                db,
                boot_id,
                settings,
                events: VecDeque::new(),
                terminal_events: VecDeque::new(),
                // Nothing is running at boot: `done` keeps a fresh client idle
                // until a real turn publishes its id (BUG-098/099).
                turn_state: json!({"turn":null,"text":"","done":true}),
                next_event,
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
            command_queue: Mutex::new(VecDeque::new()),
            assistant_dir: Mutex::new(None),
            generation: AtomicU64::new(0),
            config_version: AtomicU64::new(0),
            workers: Mutex::new(Vec::new()),
            worker_locks: Mutex::new(Vec::new()),
            desired_workers: Mutex::new(HashSet::new()),
            worker_started: Mutex::new(HashMap::new()),
            worker_attempts: Mutex::new(HashMap::new()),
            launch_path_cache: Mutex::new(None),
            base_env_cache: Mutex::new(None),
            harness_cache: Mutex::new(None),
            external_cache: Mutex::new(None),
            harness_seen: Mutex::new(BTreeSet::new()),
            state_marks: Mutex::new(HashMap::new()),
            status_cache: sessions::StatusCache::default(),
            tail_plan: Mutex::new(None),
        });
        let warm = Arc::clone(&app);
        std::thread::spawn(move || warm.warm_launch());
        // One tail sampler for the daemon: it reads the pane a prompt just
        // opened, so a delegated harness's own test result reaches the graph
        // (BUG-087).
        let sampler = Arc::clone(&app);
        std::thread::spawn(move || sampler.tail_sampler_loop());
        app.event("ready", "Rust orchestration service ready", json!({}));
        if !app.isolated {
            // Ensure a fresh install still has the Navi agent OpenCode loads.
            let prompt = app.settings()["system_prompt"]
                .as_str()
                .unwrap_or(DEFAULT_NAVI_PROMPT)
                .to_string();
            if let Err(error) = app.write_navi_agent(&prompt) {
                app.event(
                    "error",
                    "Could not write the OpenCode agent file",
                    json!({"error":error}),
                );
            }
        }
        if !app.isolated && app.settings()["microphone"].as_bool() == Some(true) {
            // Warm the decision model while the voice worker opens the mic so
            // the first spoken command does not wait for a cold model start.
            let warm = app.clone();
            std::thread::spawn(move || warm.warm_chief());
        }
        // Boot reconciliation: panes that outlived the previous daemon get
        // their rows corrected (and final screens captured) before the UI asks.
        if let Err(error) = app.reconcile_sessions() {
            app.event(
                "error",
                "Session reconciliation failed at boot",
                json!({"error":error}),
            );
        }
        Ok(app)
    }

    pub fn event(&self, kind: &str, message: &str, details: Value) {
        self.event_for(None, None, kind, message, details);
    }

    pub fn event_for(
        &self,
        project_id: Option<&str>,
        session_id: Option<&str>,
        kind: &str,
        message: &str,
        details: Value,
    ) {
        let mut details = details;
        if details.is_object()
            && details.get("request_id").is_none()
            && let Some(request) = REQUEST.with(|cell| cell.borrow().clone())
        {
            details["request_id"] = json!(request);
        }
        let mut state = self.state.lock().unwrap();
        state.next_event += 1;
        let seq = state.next_event;
        let timestamp = now();
        let boot = state.boot_id.clone();
        // A terminal reply closes its turn inside the same critical section that
        // publishes it, so no poll can observe a finished answer with `busy`
        // still set (BUG-098).
        if kind == "reply"
            && let Some(turn) = details["turn"].as_str()
            && state.turn_state["turn"].as_str() == Some(turn)
        {
            state.turn_state["text"] = details["text"].clone();
            state.turn_state["done"] = json!(true);
            state.turn_state["finished"] = json!(timestamp);
        }
        if kind != "reply_delta" {
            let text = details.to_string();
            let _ = state.db.execute(
                "INSERT INTO events(seq,epoch,project_id,session_id,time,kind,message,details)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                rusqlite::params![
                    seq as i64,
                    boot,
                    project_id,
                    session_id,
                    timestamp as i64,
                    kind,
                    message,
                    text
                ],
            );
        }
        let event = json!({"seq":seq,"time":timestamp,"epoch":boot,"kind":kind,"message":message,
                "project_id":project_id,"session_id":session_id,"details":details});
        state.events.push_back(event.clone());
        while state.events.len() > EVENT_LIMIT {
            state.events.pop_front();
        }
        if terminal_kind(kind) {
            state.terminal_events.push_back(event);
            while state.terminal_events.len() > TERMINAL_LIMIT {
                state.terminal_events.pop_front();
            }
        }
    }

    /// Open a coalesced turn. Its cumulative text is authoritative in every
    /// `status` until `turn_finish` closes it, so a client that missed events
    /// still converges on the exact answer (BUG-098).
    pub fn begin_turn(&self) -> String {
        // Time for ordering, randomness so two turns in the same millisecond
        // still get distinct ids (the client keys its row on this).
        let turn = format!("turn-{}-{}", now(), &id()[..4]);
        let mut state = self.state.lock().unwrap();
        state.turn_state = json!({"turn":turn,"text":"","done":false,"started":now()});
        turn
    }

    /// Publish the whole turn so far as a replacement payload: one event per
    /// coalescing window instead of one per model chunk.
    pub fn turn_flush(&self, turn: &str, text: &str, persona: &str, project_id: Option<&str>) {
        {
            let mut state = self.state.lock().unwrap();
            if state.turn_state["turn"].as_str() == Some(turn) {
                state.turn_state["text"] = json!(text);
                state.turn_state["done"] = json!(false);
            }
        }
        self.event_for(
            project_id,
            None,
            "reply_delta",
            "",
            json!({"turn":turn,"text":text,"done":false,"persona":persona}),
        );
    }

    /// Close the turn. `done` is the only signal a client needs to clear busy,
    /// so the error paths call this too (BUG-098).
    pub fn turn_finish(&self, turn: &str, text: &str) {
        let mut state = self.state.lock().unwrap();
        if state.turn_state["turn"].as_str() == Some(turn) {
            state.turn_state["text"] = json!(text);
            state.turn_state["done"] = json!(true);
            state.turn_state["finished"] = json!(now());
        }
    }

    /// What the host push and diagnostics read: the open turn and its truth.
    pub fn turn_snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        let turn_state = state.turn_state.clone();
        let busy = turn_state["done"].as_bool() == Some(false) && !turn_state["turn"].is_null();
        json!({"turn_state":turn_state,"busy":busy})
    }

    pub fn settings(&self) -> Value {
        self.state.lock().unwrap().settings.clone()
    }

    fn save_settings(&self, mut settings: Value, allow_profile_removal: bool) -> Result<Value> {
        if settings["microphone"].as_bool().is_none() {
            return Err("Microphone setting must be true or false".into());
        }
        if settings["speak"].as_bool().is_none() {
            return Err("Speak setting must be true or false".into());
        }
        let prompt = settings["system_prompt"]
            .as_str()
            .ok_or("Missing system prompt")?
            .to_string();
        if prompt.trim().is_empty() {
            return Err("System prompt cannot be empty".into());
        }
        if prompt.len() > 8000 {
            return Err("System prompt must be at most 8000 bytes".into());
        }
        let assistant = &settings["assistant"];
        let model_ref = assistant["model"]
            .as_str()
            .ok_or("Missing assistant model")?;
        let (provider, model_id) = model_ref
            .split_once('/')
            .ok_or("Assistant model must look like provider/model")?;
        if provider.is_empty() || model_id.is_empty() || model_ref.chars().any(char::is_whitespace)
        {
            return Err("Invalid assistant model reference".into());
        }
        let agent = assistant["agent"]
            .as_str()
            .ok_or("Missing assistant agent")?;
        if agent.is_empty()
            || agent.len() > 64
            || !agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
        {
            return Err("Invalid assistant agent id".into());
        }
        if self.isolated && settings["microphone"] == true {
            return Err("Microphone capture is disabled in isolated mode".into());
        }
        let model = settings["model"].clone();
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
        // Profile v2 (BUG-070/077/078): ids are stable, env is validated but
        // never logged, sizes/args are bounded, disappeared ids orphan their
        // sessions unless the caller opted into removal.
        let mut seen_ids: BTreeSet<String> = BTreeSet::new();
        let mut normalized_profiles = Vec::new();
        for profile in settings["profiles"].as_array().unwrap() {
            let mut profile = profile.clone();
            let pid = profile["id"]
                .as_str()
                .map(str::to_string)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| {
                    let slug: String = profile["name"]
                        .as_str()
                        .unwrap_or("profile")
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() {
                                c.to_ascii_lowercase()
                            } else {
                                '-'
                            }
                        })
                        .collect::<String>()
                        .split('-')
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("-");
                    format!("{}-{}", slug, &id()[..4])
                });
            if pid.len() > 80
                || !pid
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
            {
                return Err(format!("Invalid profile id {pid}"));
            }
            if !seen_ids.insert(pid.clone()) {
                return Err(format!("Duplicate profile id {pid}"));
            }
            profile["id"] = json!(pid);
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
            if profile["args"]
                .as_array()
                .is_some_and(|a| {
                    a.iter().any(|arg| {
                        let text = arg.as_str().unwrap_or_default();
                        text.len() > 4096 || text.contains('\0')
                    })
                })
            {
                return Err("A launch argument is invalid".into());
            }
            if !matches!(profile["args_mode"].as_str(), None | Some("argv" | "shell")) {
                return Err("args_mode must be argv or shell".into());
            }
            if let Some(env) = profile["env"].as_object() {
                if env.len() > 64 {
                    return Err("Too many profile environment entries".into());
                }
                for (key, value) in env {
                    if !crate::adapters::valid_env_key(key) {
                        return Err(format!("Invalid environment name {key}"));
                    }
                    if key.starts_with("TMUX") {
                        return Err("Profile environment may not set TMUX*".into());
                    }
                    if !value.is_string()
                        || value
                            .as_str()
                            .is_some_and(|v| v.len() > 4096 || v.contains('\0'))
                    {
                        return Err(format!("Invalid value for environment name {key}"));
                    }
                }
            } else if !profile["env"].is_null() {
                return Err("Profile environment must be an object".into());
            }
            if !matches!(profile["cwd"].as_str(), None | Some("inherit" | "project")) {
                return Err("Profile cwd must be inherit or project".into());
            }
            if !matches!(
                profile["resume_default"].as_str(),
                None | Some("ask" | "new" | "last")
            ) {
                return Err("resume_default must be ask, new, or last".into());
            }
            if let Some(size) = profile["size"].as_object() {
                let cols_ok = size["cols"].as_u64().is_none_or(|c| (20..=500).contains(&c));
                let rows_ok = size["rows"].as_u64().is_none_or(|r| (8..=200).contains(&r));
                if !(cols_ok && rows_ok) {
                    return Err("Profile size is out of range".into());
                }
            } else if !profile["size"].is_null() {
                return Err("Profile size must be an object".into());
            }
            if let Some(keys) = profile["interrupt_keys"].as_array() {
                if !keys.iter().all(Value::is_string) || keys.len() > 4 {
                    return Err("interrupt_keys must be up to 4 strings".into());
                }
            } else if !profile["interrupt_keys"].is_null() {
                return Err("interrupt_keys must be an array".into());
            }
            if profile["harness"].is_null() {
                profile["harness"] = json!(std::path::Path::new(executable)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("shell"));
            }
            normalized_profiles.push(profile);
        }
        let old_profile_ids: BTreeSet<String> = self.settings()["profiles"]
            .as_array()
            .map(|profiles| {
                profiles
                    .iter()
                    .filter_map(|p| p["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let removed: Vec<String> = old_profile_ids
            .iter()
            .filter(|pid| !seen_ids.contains(*pid))
            .cloned()
            .collect();
        if !removed.is_empty() && !allow_profile_removal {
            return Err(format!(
                "Profiles {} would be removed; pass allow_profile_removal to orphan their sessions",
                removed.join(", ")
            ));
        }
        settings["profiles"] = Value::Array(normalized_profiles);
        if let Some(map) = settings.as_object_mut() {
            map.remove("allow_profile_removal");
        }
        let old = self.settings();
        if old["system_prompt"] != settings["system_prompt"] && !self.isolated {
            self.write_navi_agent(&prompt)?;
        }
        if ["model", "profiles", "projects", "favorites"]
            .iter()
            .any(|key| old[key] != settings[key])
        {
            self.config_version.fetch_add(1, Ordering::SeqCst);
            if ["profiles", "projects"]
                .iter()
                .any(|key| old[key] != settings[key])
            {
                self.state.lock().unwrap().browser_queue.clear();
            }
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
        store::sync_settings_projects(&state.db, &settings)?;
        if !removed.is_empty() {
            // BUG-078: sessions bound to a removed profile keep their history
            // but are flagged so the UI offers a re-link action.
            for pid in &removed {
                let _ = state.db.execute(
                    "UPDATE sessions SET orphan=1 WHERE profile=?1",
                    [pid],
                );
            }
        }
        state
            .db
            .execute(
                "INSERT OR REPLACE INTO settings VALUES(1,?1)",
                [settings.to_string()],
            )
            .map_err(|e| e.to_string())?;
        state.settings = settings;
        // BUG-070: the pane base-env snapshot refreshes on settings save.
        *self.base_env_cache.lock().unwrap() = None;
        Ok(state.settings.clone())
    }

    /// Write the editable Navi system prompt as an OpenCode agent file.
    /// OpenCode watches the agents directory, so the next turn uses the edit.
    fn write_navi_agent(&self, prompt: &str) -> Result<()> {
        let home = std::env::var("HOME").map_err(|error| error.to_string())?;
        let directory = PathBuf::from(home).join(".config/opencode/agents");
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let target = directory.join(AGENT_FILE);
        let text = format!(
            "---\ndescription: Navi, the coding agent persona for the Navi desktop app\nmode: primary\n---\n{}\n",
            prompt.trim()
        );
        // Rewrite only when the stored prompt actually changed: the file is
        // content-compared so a daemon restart alone never touches it.
        if fs::read_to_string(&target).as_deref().ok() == Some(text.as_str()) {
            return Ok(());
        }
        let temporary = directory.join(format!("{AGENT_FILE}.new"));
        fs::write(&temporary, text).map_err(|error| error.to_string())?;
        fs::rename(&temporary, &target).map_err(|error| error.to_string())
    }

    pub fn set_active(&self, session: &str) -> Result<()> {
        let mut settings = self.settings();
        settings["active_session"] = json!(session);
        self.save_settings(settings, false)?;
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

    /// BUG-056: the UI settles a held transcript. Accepting is an atomic
    /// `create_new` claim, so when the worker already won the deadline race the
    /// UI is told not to dispatch; cancelling withdraws the claim instead.
    fn voice_confirm(&self, p: &Value) -> Result<Value> {
        let accept = p["accept"].as_bool().ok_or("Missing accept flag")?;
        let claim = self.runtime.join("voice.claim");
        let confirm = self.runtime.join("voice.confirm");
        if accept {
            return match OpenOptions::new().write(true).create_new(true).open(&claim) {
                Ok(mut file) => {
                    let _ = file.write_all(now().to_string().as_bytes());
                    let _ = fs::remove_file(&confirm);
                    Ok(json!({"claimed":true}))
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(json!({"claimed":false,"already":true}))
                }
                Err(error) => Err(error.to_string()),
            };
        }
        fs::write(&confirm, json!({"accept":false}).to_string())
            .map_err(|error| error.to_string())?;
        match fs::remove_file(&claim) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(json!({"cancelled":true}))
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
        // Isolated tests cannot open a mic or a desktop socket; the one hook is a
        // namespaced fake worker so supervision itself remains testable.
        let script = if self.isolated {
            match std::env::var("NAVI_TEST_WORKER_SCRIPT") {
                Ok(path) => PathBuf::from(path),
                Err(_) => {
                    return Err(
                        "Live desktop and microphone workers are disabled in isolated mode".into(),
                    );
                }
            }
        } else {
            self.root.join(format!("workers/{worker}.py"))
        };
        let mut workers = self.workers.lock().unwrap();
        workers.retain_mut(|(_, child)| child.try_wait().ok().flatten().is_none());
        if let Some((_, child)) = workers.iter_mut().find(|(name, _)| name == worker)
            && child.try_wait().ok().flatten().is_none()
        {
            let pid = child.id() as i32;
            self.desired_workers.lock().unwrap().insert(worker.to_string());
            self.worker_started
                .lock()
                .unwrap()
                .entry(worker.to_string())
                .or_insert_with(now);
            return Ok(json!({"running":true,"pid":pid}));
        }
        let pid_file = self.runtime.join(format!("{worker}.pid"));
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok())
            && pid_is_worker(pid, worker)
        {
            self.desired_workers.lock().unwrap().insert(worker.to_string());
            self.worker_started
                .lock()
                .unwrap()
                .entry(worker.to_string())
                .or_insert_with(now);
            return Ok(json!({"running":true,"pid":pid}));
        }
        let _ = fs::remove_file(&pid_file);
        // A slot holds its lock for the child's whole life: a duplicate start is
        // impossible even if the pid file was lost. Replacing the entry drops
        // the previous lock when a dead worker is restarted.
        self.worker_locks
            .lock()
            .unwrap()
            .retain(|(name, _)| name != worker);
        let lock_path = self.runtime.join(format!("{worker}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| error.to_string())?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!("The {worker} worker is already running"));
        }
        let child = Command::new(if worker == "desktop" {
            PathBuf::from("/usr/bin/python3")
        } else {
            self.python()
        })
        .arg(script)
        .env("NAVI_RUNTIME_DIR", &self.runtime)
        .env("NAVI_DATA_DIR", &self.data)
        .env("NEEDLE_TELEMETRY", "0")
        .env("DO_NOT_TRACK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| error.to_string())?;
        let pid = child.id() as i32;
        if let Err(error) = fs::write(&pid_file, pid.to_string()) {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Could not record the worker pid: {error}"));
        }
        workers.push((worker.to_string(), child));
        self.worker_locks
            .lock()
            .unwrap()
            .push((worker.to_string(), lock));
        self.desired_workers.lock().unwrap().insert(worker.to_string());
        self.worker_started
            .lock()
            .unwrap()
            .insert(worker.to_string(), now());
        Ok(json!({"running":true,"pid":pid}))
    }

    /// Restart crashed workers with 1/2/5/10 s backoff, five attempts, and a
    /// 60 s healthy window that resets the counter (BUG-063).
    pub fn supervise(&self) {
        let mut exits = Vec::new();
        {
            let mut workers = self.workers.lock().unwrap();
            workers.retain_mut(|(name, child)| match child.try_wait() {
                Ok(Some(status)) => {
                    exits.push((name.clone(), status.to_string()));
                    false
                }
                _ => true,
            });
        }
        for (name, status) in exits {
            self.event(
                "worker_restart",
                &format!("{name} worker exited ({status})"),
                json!({"worker":name,"status":status}),
            );
        }
        let desired = self.desired_workers.lock().unwrap().clone();
        for name in desired {
            let alive = {
                let mut workers = self.workers.lock().unwrap();
                workers.iter_mut().any(|(worker, child)| {
                    worker == &name && child.try_wait().ok().flatten().is_none()
                })
            };
            if alive {
                let started = self
                    .worker_started
                    .lock()
                    .unwrap()
                    .get(&name)
                    .copied()
                    .unwrap_or(0);
                if now().saturating_sub(started) > 60_000 {
                    self.worker_attempts.lock().unwrap().remove(&name);
                }
                continue;
            }
            let (attempts, next) = self
                .worker_attempts
                .lock()
                .unwrap()
                .get(&name)
                .copied()
                .unwrap_or((0, 0));
            if now() < next {
                continue;
            }
            if attempts >= 5 {
                self.desired_workers.lock().unwrap().remove(&name);
                self.event(
                    "error",
                    &format!("{name} worker keeps crashing; giving up"),
                    json!({"worker":name}),
                );
                continue;
            }
            let delay = [1000, 2000, 5000, 10000, 10000][attempts.min(4) as usize];
            self.worker_attempts
                .lock()
                .unwrap()
                .insert(name.clone(), (attempts + 1, now() + delay));
            if let Err(error) = self.spawn_worker(&name) {
                self.event(
                    "error",
                    &format!("{name} worker restart failed: {error}"),
                    json!({"worker":name}),
                );
            }
        }
    }

    /// Stop every child we own and clear its pid file. Called on SIGTERM/INT.
    pub fn shutdown(&self) {
        let mut workers = self.workers.lock().unwrap();
        for (_name, mut child) in workers.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.desired_workers.lock().unwrap().clear();
        self.worker_started.lock().unwrap().clear();
        self.worker_attempts.lock().unwrap().clear();
        for (worker, _lock) in self.worker_locks.lock().unwrap().drain(..) {
            let _ = fs::remove_file(self.runtime.join(format!("{worker}.pid")));
        }
    }

    fn worker_snapshot(&self) -> Value {
        let locks = self.worker_locks.lock().unwrap();
        let desired = self.desired_workers.lock().unwrap();
        let started = self.worker_started.lock().unwrap();
        // Union of desired workers and lock holders: during a supervisor
        // restart window a wanted worker has no lock yet, and a pid-adopted
        // worker never takes one; neither may vanish from status.
        let mut names: HashSet<&String> = locks.iter().map(|(name, _)| name).collect();
        names.extend(desired.iter());
        let mut workers = serde_json::Map::new();
        for worker in names {
            let pid = fs::read_to_string(self.runtime.join(format!("{worker}.pid")))
                .ok()
                .and_then(|value| value.trim().parse::<i32>().ok());
            workers.insert(
                worker.clone(),
                json!({"pid":pid,
                    "alive":pid.map(|pid| pid_is_worker(pid, worker)).unwrap_or(false),
                    "since":started.get(worker).copied()}),
            );
        }
        Value::Object(workers)
    }

    fn dispatch(&self, request: Value) -> Result<Value> {
        let p = &request["params"];
        match request["op"].as_str().ok_or("Missing operation")? {
            "ping" => Ok(json!({"version":"0.1.0","pid":std::process::id()})),
            "status" => {
                // `?sections=` turns a poll into a delta: only the sections the
                // caller asks for are computed, and an unchanged version is
                // omitted instead of re-sent (BUG-094). No sections parameter
                // keeps the full payload every existing caller expects.
                let sections: Option<Vec<String>> = p["sections"].as_array().map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                });
                let known = p["versions"].as_object();
                let selective = sections.is_some();
                let wanted = |name: &str| {
                    sections
                        .as_ref()
                        .is_none_or(|list| list.iter().any(|item| item == name))
                };
                let known_version =
                    |name: &str| known.and_then(|map| map.get(name)).and_then(Value::as_u64);
                let mut payload = serde_json::Map::new();
                let mut versions = serde_json::Map::new();
                if wanted("sessions") {
                    let sessions = Value::Array(self.sessions()?);
                    section(&mut payload, &mut versions, "sessions", sessions,
                        known_version("sessions"), selective);
                }
                if wanted("active_session_tail") {
                    let tail = self.active_session_tail();
                    section(&mut payload, &mut versions, "active_session_tail", tail,
                        known_version("active_session_tail"), selective);
                }
                if wanted("projects") {
                    let projects = Value::Array(self.projects()?);
                    section(&mut payload, &mut versions, "projects", projects,
                        known_version("projects"), selective);
                }
                if wanted("workers") {
                    let workers = self.worker_snapshot();
                    section(&mut payload, &mut versions, "workers", workers,
                        known_version("workers"), selective);
                }
                if wanted("cache") {
                    let cache = self.status_cache.snapshot();
                    section(&mut payload, &mut versions, "cache", cache,
                        known_version("cache"), selective);
                }
                let mut s = self.state.lock().unwrap();
                if p["gui"].as_bool() == Some(true) {
                    s.gui_seen = now();
                }
                let since = p["since"].as_u64().unwrap_or(0).min(i64::MAX as u64);
                let ram_min = s
                    .events
                    .iter()
                    .filter_map(|event| event["seq"].as_u64())
                    .min();
                let db_min = s
                    .db
                    .query_row("SELECT COALESCE(MIN(seq),0) FROM events", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap_or(0) as u64;
                // The oldest seq this daemon can still serve. A cursor below it
                // is unrecoverable, so the client resyncs (BUG-099).
                let events_from = ram_min.map_or(db_min, |min| min.min(db_min));
                let mut page: BTreeMap<u64, Value> = BTreeMap::new();
                // Fast path: while the cursor sits inside the RAM window there is
                // nothing to read from disk, so a 40 ms poll costs no query.
                if !ram_min.is_some_and(|min| since + 1 >= min) {
                    let ceiling = ram_min
                        .map(|value| value.min(i64::MAX as u64) as i64)
                        .unwrap_or(i64::MAX);
                    let mut statement = s
                        .db
                        .prepare(
                            "SELECT seq,epoch,project_id,session_id,time,kind,message,details
                             FROM events WHERE seq>?1 AND seq<?2 ORDER BY seq LIMIT ?3",
                        )
                        .map_err(|error| error.to_string())?;
                    let rows = statement
                        .query_map(
                            rusqlite::params![since as i64, ceiling, (EVENT_PAGE + 1) as i64],
                            |row| {
                                Ok(json!({
                                    "seq": row.get::<_, i64>(0)?,
                                    "epoch": row.get::<_, String>(1)?,
                                    "project_id": row.get::<_, Option<String>>(2)?,
                                    "session_id": row.get::<_, Option<String>>(3)?,
                                    "time": row.get::<_, u64>(4)?,
                                    "kind": row.get::<_, String>(5)?,
                                    "message": row.get::<_, String>(6)?,
                                    "details": serde_json::from_str::<Value>(&row.get::<_, String>(7)?)
                                        .unwrap_or(json!({})),
                                }))
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    for row in rows {
                        let event = row.map_err(|error| error.to_string())?;
                        if let Some(seq) = event["seq"].as_u64() {
                            page.insert(seq, event);
                        }
                    }
                }
                // RAM first: it holds the coalesced deltas the DB never stores;
                // the terminal tier is merged second so it is never dropped.
                for event in s.events.iter().chain(s.terminal_events.iter()) {
                    if let Some(seq) = event["seq"].as_u64()
                        && seq > since
                    {
                        page.entry(seq).or_insert_with(|| event.clone());
                    }
                }
                let mut events: Vec<Value> = page.into_values().collect();
                let has_more = events.len() > EVENT_PAGE;
                events.truncate(EVENT_PAGE);
                let turn_state = s.turn_state.clone();
                let busy =
                    turn_state["done"].as_bool() == Some(false) && !turn_state["turn"].is_null();
                let boot_id = s.boot_id.clone();
                let settings = s.settings.clone();
                let browser = s.browser.clone();
                let desktop = s.desktop.clone();
                let voice = s.voice.clone();
                let browser_connected = now().saturating_sub(s.browser_seen) < 3000;
                let gui_open = now().saturating_sub(s.gui_seen) < 2000;
                drop(s);
                if wanted("settings") {
                    section(&mut payload, &mut versions, "settings", settings,
                        known_version("settings"), selective);
                }
                if wanted("browser") {
                    section(&mut payload, &mut versions, "browser", browser,
                        known_version("browser"), selective);
                }
                if wanted("desktop") {
                    section(&mut payload, &mut versions, "desktop", desktop,
                        known_version("desktop"), selective);
                }
                if wanted("voice") {
                    section(&mut payload, &mut versions, "voice", voice,
                        known_version("voice"), selective);
                }
                payload.insert("isolated".into(), json!(self.isolated));
                payload.insert("boot_id".into(), json!(boot_id));
                payload.insert("events_from".into(), json!(events_from));
                // A truncated page is drained by the client immediately rather
                // than waiting for the next poll (BUG-098/099).
                payload.insert("has_more".into(), json!(has_more));
                payload.insert(
                    "generation".into(),
                    json!(self.generation.load(Ordering::SeqCst)),
                );
                payload.insert(
                    "config_version".into(),
                    json!(self.config_version.load(Ordering::SeqCst)),
                );
                payload.insert("gui_open".into(), json!(gui_open));
                payload.insert("browser_connected".into(), json!(browser_connected));
                payload.insert("turn_state".into(), turn_state);
                payload.insert("busy".into(), json!(busy));
                payload.insert("events".into(), Value::Array(events));
                payload.insert("versions".into(), Value::Object(versions));
                Ok(Value::Object(payload))
            }
            "settings" => self.save_settings(
                p.clone(),
                p["allow_profile_removal"].as_bool() == Some(true),
            ),
            "projects" => Ok(json!({"projects":self.projects()?})),
            "project_add" => {
                let name = p["name"].as_str().ok_or("Missing project name")?;
                let path = p["path"].as_str().ok_or("Missing project path")?;
                self.add_project(name, path)
                    .map(|project| json!({"project":project}))
            }
            "project_probe" => {
                let id = p["id"].as_str().ok_or("Missing project")?;
                self.probe_project(id)
                    .map(|project| json!({"project":project}))
            }
            "graph_get" => self.graph_get(p["project_id"].as_str()),
            "aspect_add" => self.add_aspect(p),
            "aspect_set" => self.set_aspect(p),
            "task_add" => self.add_task(p),
            "task_set" => self.set_task(p),
            "plan_add" => self.add_plan(p),
            "plan_set" => self.set_plan(p),
            "checkpoint_add" => self.add_checkpoint(p),
            "progress_claim" => self.progress_claim(p),
            "progress_report" => self.progress_report(p),
            "progress_get" => {
                let project_id = self.progress_project(p)?;
                let graph = self.graph_get(Some(&project_id))?;
                Ok(json!({"project_id":project_id,"text":store::progress_text(&graph),
                    "summary":store::progress_sentence(&graph),"graph":graph}))
            }
            "progress_overview" => self.progress_overview(),
            "link_setup" => link::LINK_HARNESSES
                .into_iter()
                .map(|harness| {
                    link::setup(harness).map(|(title, place, snippet)| {
                        json!({"harness": harness, "title": title, "place": place, "snippet": snippet})
                    })
                })
                .collect::<Result<Vec<_>>>()
                .map(|harnesses| json!({"harnesses": harnesses})),
            "project_scan" => self.project_scan(p),
            "project_detect" => self.project_detect(p),
            "history" => self.history(p),
            "message_save" => self.save_message(p),
            "messages" => self.messages(p),
            "session_events" => self.session_events(p),
            "window_state" => self.window_state(p),
            "create" => self.create_session(p),
            "harnesses" | "harness_list" => self.harness_list(),
            "session_output" => self.session_output(p),
            "harness_tail" => self.harness_tail(p),
            "session_discover" => self.session_discover(),
            "session_resume" => self.session_resume(p),
            "session_kill" => self.session_kill(p),
            "session_remove" => self.session_remove(p),
            "session_restart" => self.session_restart(p),
            "session_interrupt" => self.session_interrupt(p),
            "session_target" => self.session_target(p),
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
                self.session_target(&json!({"id":target}))
            }
            "prompt" => {
                let target = p["id"].as_str().ok_or("Missing session")?;
                let text = p["text"].as_str().ok_or("Missing prompt")?;
                // Attachments validate here but cannot travel through a PTY;
                // the assistant path ("command") carries them (BUG-005).
                chief::validate_attachments(&p["attachments"])?;
                self.inject(target, text, true)?;
                Ok(json!({"sent":true,"id":target}))
            }
            "command" | "decide" => {
                let text = p["text"].as_str().ok_or("Missing command")?;
                if text.is_empty() || text.len() > 2000 {
                    return Err("Command must contain 1 to 2000 bytes".into());
                }
                chief::validate_attachments(&p["attachments"])?;
                if request["op"] == "command"
                    && let Some(persona) = self.persona_switch(text)?
                {
                    return Ok(
                        json!({"trace":[],"dry_run":false,"reply":persona.reply,"persona":persona.name}),
                    );
                }
                let generation = self.generation.load(Ordering::SeqCst);
                if request["op"] == "decide" {
                    // Read-only dry run: lock-free, never queued (BUG-018).
                    return self.run_command(text, p, true, generation);
                }
                let mut params = p.clone();
                params["text"] = json!(text);
                self.run_or_queue(&params, generation)
            }
            "cancel" => {
                self.generation.fetch_add(1, Ordering::SeqCst);
                let _ = fs::write(self.runtime.join("voice.cancel"), now().to_string());
                let dropped = self.command_queue.lock().unwrap().drain(..).count();
                self.state.lock().unwrap().browser_queue.clear();
                self.event(
                    "stopped",
                    "Current command stopped",
                    json!({"queued_dropped":dropped}),
                );
                // Interrupt the server-side turn and report whether OpenCode
                // confirmed; a false interrupts nothing further (BUG-007).
                let interrupted = self.gateway_interrupt();
                if !interrupted {
                    self.event(
                        "warning",
                        "Assistant turn may still be running: OpenCode did not confirm the interrupt",
                        json!({"interrupted":false}),
                    );
                }
                // BUG-082: cancel is state-aware — busy harness panes get a
                // soft interrupt alongside the gateway interrupt.
                let harness_hits = self.interrupt_busy_harnesses();
                Ok(json!({"stopped":true,"interrupted":interrupted,"harnesses":harness_hits}))
            }
            "voice_stop" => {
                let _ = fs::write(self.runtime.join("voice.cancel"), now().to_string());
                Ok(json!({"stopped":true}))
            }
            // BUG-056: the UI either takes the held transcript or withdraws it.
            "voice_confirm" => self.voice_confirm(p),
            "oc_models" => self.gateway_op(&["models"]),
            "oc_agents" => self.gateway_op(&["agents"]),
            "oc_commands" => self.gateway_op(&["commands"]),
            "oc_new" => {
                // New sessions are scoped to the resolved project (BUG-088).
                let project = self.assistant_project(p["project_id"].as_str())?;
                let path = project["path"].as_str().unwrap_or_default();
                self.gateway_op(&["new", "--directory", path])
            }
            // The assistant session follows the project switcher (BUG-090):
            // the next turn, interrupt, and permission reply all use this
            // directory, without starting a turn or a new session.
            "assistant_scope" => self.assistant_scope(p),
            "oc_interrupt" => Ok(json!({"interrupted": self.gateway_interrupt()})),
            "permission_reply" => {
                let id = p["id"].as_str().ok_or("Missing permission id")?;
                let decision = p["decision"].as_str().ok_or("Missing decision")?;
                self.permission_reply(id, decision)
            }
            "voice_start" => self.spawn_worker("voice"),
            "desktop_start" => self.spawn_worker("desktop"),
            "desktop_stop" => {
                let pid_file = self.runtime.join("desktop.pid");
                if let Ok(pid) = fs::read_to_string(&pid_file)
                    .and_then(|value| value.parse::<u32>().map_err(std::io::Error::other))
                {
                    let mut workers = self.workers.lock().unwrap();
                    if let Some(index) = workers
                        .iter()
                        .position(|(_, child)| child.id() == pid)
                    {
                        let (_, mut child) = workers.remove(index);
                        let _ = child.kill();
                        child.wait().map_err(|error| error.to_string())?;
                    }
                    self.desired_workers.lock().unwrap().remove("desktop");
                    self.worker_started.lock().unwrap().remove("desktop");
                    self.worker_attempts.lock().unwrap().remove("desktop");
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

fn response(app: &App, mut request: Value) -> Value {
    let op = request["op"].as_str().unwrap_or_default().to_string();
    // A command keeps one id from its RPC reply through every event it causes.
    let command = op == "command" && request["params"].is_object();
    if command {
        request["params"]["request_id"] = json!(command_request_id(&request["params"]["request_id"]));
    }
    let request_id = request["params"]["request_id"].as_str().map(str::to_string);
    match app.dispatch(request) {
        Ok(mut value) => {
            if command && let Some(id) = &request_id {
                if value.is_object() {
                    value["request_id"] = json!(id);
                }
                if value["reply"].is_object() {
                    value["reply"]["request_id"] = json!(id);
                }
            }
            json!({"ok":true,"result":value})
        }
        Err(error) => {
            // The op lets a client tell its conversation's failure from another
            // caller's (a harness's progress call, a panel edit).
            app.event("error", &error, json!({"op":op,"request_id":request_id}));
            json!({"ok":false,"error":error})
        }
    }
}

static TERMINATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn request_termination(_: libc::c_int) {
    TERMINATE.store(true, Ordering::SeqCst);
}

fn pid_is_worker(pid: i32, worker: &str) -> bool {
    if pid <= 1 || unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| String::from_utf8_lossy(&bytes).contains(&format!("{worker}.py")))
        .unwrap_or(false)
}

fn serve() -> Result<()> {
    let app = App::new()?;
    unsafe {
        libc::signal(
            libc::SIGTERM,
            request_termination as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            request_termination as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let watchdog = Arc::clone(&app);
    std::thread::spawn(move || loop {
        if TERMINATE.load(Ordering::SeqCst) {
            watchdog.shutdown();
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    });
    let prober = Arc::clone(&app);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        loop {
            if let Ok(projects) = prober.projects() {
                for project in projects {
                    if let Some(id) = project["id"].as_str() {
                        let _ = prober.probe_project(id);
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    let supervisor = Arc::clone(&app);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        supervisor.supervise();
    });
    // Queued commands run under the command lock from this drain thread
    // (BUG-016); the thread is idle whenever the queue is empty.
    let drainer = Arc::clone(&app);
    std::thread::spawn(move || drainer.drain_commands());
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
    app.shutdown();
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
    // Client commands print and exit, so a closed pipe (`navid link | head`)
    // should end them quietly; Rust's ignored SIGPIPE turns it into a panic.
    // The daemons keep ignoring it so a vanished peer is an I/O error.
    if !matches!(args.get(1).map(String::as_str), Some("serve" | "native")) {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let result = match args.get(1).map(String::as_str) {
        Some("serve") => serve(),
        Some("native") => native(),
        Some("mcp") => link::mcp(),
        Some("progress") => link::progress(&args[2..]),
        Some("link") => link::link(&args[2..]),
        Some("rpc") => serde_json::from_str(
            args.get(2)
                .map(String::as_str)
                .unwrap_or("{\"op\":\"status\"}"),
        )
        .map_err(|e| e.to_string())
        .and_then(rpc)
        .map(|value| println!("{value}")),
        _ => {
            println!(
                "navid serve                   run the station daemon
navid native                  browser native-messaging host
navid mcp                     MCP server a harness starts to report progress
navid progress [report|task]  read or report progress from a shell (navid progress -h)
navid link [harness]          print the setup for a harness started outside Navi
navid rpc '<JSON request>'    send one request to the daemon"
            );
            Ok(())
        }
    };
    if let Err(error) = result {
        eprintln!("Navi: {error}");
        std::process::exit(1);
    }
}
