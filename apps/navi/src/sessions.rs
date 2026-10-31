use crate::{App, Result, id, now};
use rusqlite::params;
use serde_json::{Value, json};
use std::process::Command;

impl App {
    pub fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command.arg("-S").arg(self.data.join("terminals.sock"));
        if std::env::var("NAVI_MANAGED_TERMINALS").as_deref() == Ok("1") {
            command.arg("-N");
        }
        command
    }

    pub fn sessions(&self) -> Result<Vec<Value>> {
        let state = self.state.lock().unwrap();
        let mut query = state
            .db
            .prepare("SELECT id,name,profile,cwd,created FROM sessions ORDER BY created")
            .map_err(|e| e.to_string())?;
        let rows=query.query_map([],|row|Ok(json!({"id":row.get::<_,String>(0)?,"name":row.get::<_,String>(1)?,
            "profile":row.get::<_,String>(2)?,"cwd":row.get::<_,String>(3)?,"created":row.get::<_,u64>(4)?}))).map_err(|e|e.to_string())?;
        let mut records = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(query);
        drop(state);
        let live = self
            .tmux()
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
            .map_err(|e| e.to_string())?;
        let names = String::from_utf8_lossy(&live.stdout);
        for record in &mut records {
            record["running"] = json!(
                names
                    .lines()
                    .any(|name| Some(name) == record["id"].as_str())
            );
        }
        Ok(records)
    }

    pub fn require_session(&self, target: &str) -> Result<()> {
        if self
            .sessions()?
            .iter()
            .any(|s| s["id"] == target && s["running"] == true)
        {
            Ok(())
        } else {
            Err("Session is not running".into())
        }
    }

    pub fn create_session(&self, p: &Value) -> Result<Value> {
        let settings = self.settings();
        let profile = settings["profiles"]
            .as_array()
            .ok_or("Missing profiles")?
            .iter()
            .find(|profile| profile["id"] == p["profile"])
            .ok_or("Unknown installed launch profile")?;
        let executable = profile["executable"].as_str().ok_or("Missing executable")?;
        let cwd = p["cwd"].as_str().ok_or("Select a project directory")?;
        let path =
            std::fs::canonicalize(cwd).map_err(|e| format!("Invalid project directory: {e}"))?;
        if !path.is_dir() {
            return Err("Project path must be a directory".into());
        }
        let name = p["name"]
            .as_str()
            .unwrap_or(profile["name"].as_str().unwrap_or("Session"));
        if name.is_empty() || name.len() > 120 || name.chars().any(char::is_control) {
            return Err("Invalid session name".into());
        }
        // Resolve executable before starting tmux: a missing harness is a launch error.
        let resolved = if executable.contains('/') {
            let executable = std::fs::canonicalize(executable)
                .map_err(|e| format!("Harness not installed: {e}"))?;
            executable.to_string_lossy().into_owned()
        } else {
            std::env::var("PATH")
                .unwrap_or_default()
                .split(':')
                .map(|dir| std::path::Path::new(dir).join(executable))
                .find(|path| path.is_file())
                .ok_or(format!(
                    "{executable} is not installed or is absent from PATH"
                ))?
                .to_string_lossy()
                .into_owned()
        };
        let target = format!("navi-{}", id());
        let mut command = self.tmux();
        command
            .args(["new-session", "-d", "-s", &target, "-c"])
            .arg(&path)
            .arg("--")
            .arg(&resolved);
        for argument in profile["args"].as_array().ok_or("Missing arguments")? {
            command.arg(argument.as_str().ok_or("Arguments must be strings")?);
        }
        command.env_remove("TMUX").env_remove("TMUX_PANE");
        let output = command.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        let output = self
            .tmux()
            .args(["set-option", "-g", "status", "off"])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("Terminal server configuration failed".into());
        }
        self.tmux()
            .args(["set-option", "-g", "mouse", "on"])
            .output()
            .map_err(|e| e.to_string())?;
        self.state
            .lock()
            .unwrap()
            .db
            .execute(
                "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5)",
                params![
                    target,
                    name,
                    p["profile"].as_str().unwrap(),
                    path.to_string_lossy(),
                    now()
                ],
            )
            .map_err(|e| e.to_string())?;
        self.set_active(&target)?;
        let result =
            json!({"id":target,"name":name,"profile":p["profile"],"cwd":path,"running":true});
        self.event("launched", "Harness session launched", result.clone());
        Ok(result)
    }
}
