//! Private project worker supervisor for the opt-in owner-approved route.
//! Owner code retains approve/cancel. Models may propose request data only.
use crate::{Result,browser_host::BrowserHost};
use serde_json::{Value,json};
use std::{path::Path,process::Command,sync::Mutex,time::{Duration,Instant}};

struct Gate { closed:bool, pending:Option<String>, awaiting:Option<String>, calls:usize }
pub struct ProjectHost { host:BrowserHost, scope:String, deadline:Instant, gate:Mutex<Gate> }
impl ProjectHost {
    pub fn start(python:&Path,worker:&Path,root:&Path,scope:&str)->Result<Self> {
        if !root.is_absolute() || scope.is_empty() || scope.len()>256 || scope.chars().any(char::is_control) {
            return Err("Invalid owner-selected project scope".into());
        }
        let mut command=Command::new(python);
        command.arg("-I").arg(worker).arg(root).arg(scope);
        // -I excludes cwd/PYTHONPATH; the worker must add its own trusted sibling
        // directory, not load imports from the selected project.
        let host=BrowserHost::spawn(command)?;
        let result=Self{host,scope:scope.into(),deadline:Instant::now()+Duration::from_secs(60),
            gate:Mutex::new(Gate{closed:false,pending:None,awaiting:None,calls:0})};
        let start=Instant::now();
        loop {
            if start.elapsed()>Duration::from_secs(3) {result.cancel();return Err("Project worker startup timed out".into());}
            if let Some(record)=result.host.receive()? {
                if record["type"]=="ready" && record["scope"]==result.scope {return Ok(result);}
                result.cancel();return Err("Invalid project worker startup".into());
            }
        }
    }
    fn current(&self,gate:&mut Gate)->Result<()> {
        if gate.closed || Instant::now()>=self.deadline {gate.closed=true;self.host.stop();return Err("Project turn closed or expired".into());}
        Ok(())
    }
    pub fn request(&self,operation:&str,args:&Value)->Result<()> {
        let mut gate=self.gate.lock().unwrap();self.current(&mut gate)?;
        if gate.pending.is_some() || gate.awaiting.is_some() || gate.calls>=16 {return Err("Project request busy or exhausted".into());}
        if !matches!(operation,"read"|"list") || !args.is_object() {return Err("Invalid project proposal".into());}
        let request=json!({"operation":operation,"args":args});
        if request.to_string().len()>3000 {return Err("Project proposal exceeds limit".into());}
        gate.calls+=1;gate.awaiting=Some("permission".into());
        if let Err(error)=self.host.send(&json!({"type":"request","request":request})) {gate.closed=true;self.host.stop();return Err(error);}
        Ok(())
    }
    pub fn approve(&self,id:&str,decision:&str)->Result<()> {
        let mut gate=self.gate.lock().unwrap();self.current(&mut gate)?;
        if !matches!(decision,"allow"|"deny") || gate.pending.as_deref()!=Some(id) {return Err("Project decision absent, stale or invalid".into());}
        gate.pending=None;gate.awaiting=Some(id.into());
        if let Err(error)=self.host.send(&json!({"type":"decision","id":id,"decision":decision})) {gate.closed=true;self.host.stop();return Err(error);}
        Ok(())
    }
    /// Caller must perform its generation check again before disclosing an
    /// event: a returned value cannot be retracted after cancel wins later.
    pub fn receive(&self)->Result<Option<Value>> {
        let record=self.host.receive();
        let mut gate=self.gate.lock().unwrap();self.current(&mut gate)?;
        let record=match record {Ok(Some(v))=>v,Ok(None)=>return Ok(None),Err(e)=>{gate.closed=true;self.host.stop();return Err(e);}};
        let valid=record["scope"]==self.scope && match record["type"].as_str() {
            Some("permission")=>{
                let id=record["id"].as_str().unwrap_or("");
                if gate.awaiting.as_deref()==Some("permission") && id.starts_with("project-perm-") && id.len()==45 && record["choices"]==json!(["allow","deny"]) && matches!(record["tool"].as_str(),Some("project_read"|"project_list")) {
                    gate.awaiting=None;gate.pending=Some(id.into());true
                }else{false}
            },
            Some("project_result"|"project_error")=>{
                if record["id"].as_str()==gate.awaiting.as_deref() && gate.awaiting.as_deref()!=Some("permission") && gate.awaiting.is_some() {gate.awaiting=None;true}else{false}
            },
            _=>false,
        };
        if !valid {gate.closed=true;self.host.stop();return Err("Invalid project worker event".into());}
        Ok(Some(record))
    }
    pub fn cancel(&self) {
        let mut gate=self.gate.lock().unwrap_or_else(|e|e.into_inner());
        gate.closed=true;gate.pending=None;gate.awaiting=None;self.host.stop();
    }
}
impl Drop for ProjectHost {fn drop(&mut self){self.cancel();}}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(ProjectHost,std::path::PathBuf) {
        let root=std::env::temp_dir().join(format!("navi-project-{}",crate::id()));std::fs::create_dir(&root).unwrap();std::fs::write(root.join("one.py"),"synthetic rust pipe\n").unwrap();
        let worker=Path::new(env!("CARGO_MANIFEST_DIR")).join("workers/project_worker.py");
        let python=std::env::var_os("PATH").and_then(|p|std::env::split_paths(&p).map(|p|p.join("python3")).find(|p|p.is_file())).unwrap();
        (ProjectHost::start(&python,&worker,&root,"test-turn").unwrap(),root)
    }
    fn event(host:&ProjectHost)->Value {let start=Instant::now();loop {assert!(start.elapsed()<Duration::from_secs(3));if let Some(v)=host.receive().unwrap(){return v;}}}
    #[test] fn exact_owner_allow_and_no_replay(){let(h,r)=fixture();h.request("read",&json!({"path":"one.py"})).unwrap();let p=event(&h);let id=p["id"].as_str().unwrap();assert!(h.approve(id,"always").is_err());h.approve(id,"allow").unwrap();assert!(h.approve(id,"allow").is_err());assert_eq!(event(&h)["result"]["text"],"synthetic rust pipe\n");h.cancel();std::fs::remove_dir_all(r).unwrap();}
    #[test] fn cancel_discards_queued_worker_bytes(){let(h,r)=fixture();h.request("list",&json!({"path":""})).unwrap();std::thread::sleep(Duration::from_millis(100));h.cancel();assert!(h.receive().is_err());assert!(h.request("list",&json!({"path":""})).is_err());std::fs::remove_dir_all(r).unwrap();}
    #[test] fn denial_and_wrong_token(){let(h,r)=fixture();h.request("read",&json!({"path":"one.py"})).unwrap();let p=event(&h);assert!(h.approve("project-perm-wrong","allow").is_err());h.approve(p["id"].as_str().unwrap(),"deny").unwrap();assert_eq!(event(&h)["result"]["denied"],true);h.cancel();std::fs::remove_dir_all(r).unwrap();}
    #[test] fn request_overlap_is_refused(){let(h,r)=fixture();h.request("list",&json!({"path":""})).unwrap();assert!(h.request("list",&json!({"path":""})).is_err());h.cancel();std::fs::remove_dir_all(r).unwrap();}
}
