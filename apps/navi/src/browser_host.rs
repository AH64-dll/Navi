use crate::Result;
use serde_json::{Value,json};
use std::collections::{HashSet,HashMap};
use std::sync::Arc;
use std::io::{BufRead,BufReader,Read,Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child,ChildStdin,Command,Stdio};
use std::sync::{Mutex,mpsc};
use std::time::Duration;

pub struct BrowserHost {
    child: Arc<Mutex<Child>>,
    terminated: Arc<std::sync::atomic::AtomicBool>,
    status: Arc<Mutex<Value>>,
    input: Mutex<ChildStdin>,
    output: Mutex<mpsc::Receiver<Value>>,
    permissions: Mutex<HashSet<String>>,
    replies: Arc<Mutex<HashMap<String,mpsc::SyncSender<bool>>>>,
}
impl BrowserHost {
    pub fn start(bundle: &std::path::Path) -> Result<Self> {
        if !bundle.is_file() { return Err("Build packages/navi/scripts/package-station.mjs first".into()); }
        let node=std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory|directory.join("node")).find(|path|path.is_file())
            .and_then(|path|std::fs::canonicalize(path).ok()).ok_or("Node executable not found")?;
        let mut command=Command::new(node);
        command.arg("--").arg(bundle).env_remove("NODE_OPTIONS").env_remove("NODE_PATH");
        Self::spawn(command)
    }
    pub(crate) fn spawn(mut command:Command)->Result<Self> {
        let mut child=command.process_group(0).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().map_err(|e|e.to_string())?;
        let input=child.stdin.take().ok_or("Browser host has no input")?;
        let output=child.stdout.take().ok_or("Browser host has no output")?;
        let fd=input.as_raw_fd();
        let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};
        if flags<0 || unsafe{libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}<0 {
            unsafe{libc::kill(-(child.id() as i32),libc::SIGKILL);}
            let _=child.kill();let _=child.wait();return Err("Cannot bound browser host writes".into());
        }
        let child=Arc::new(Mutex::new(child));
        let reader_child=Arc::clone(&child);
        let terminated=Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_terminated=Arc::clone(&terminated);
        let status=Arc::new(Mutex::new(json!({"link":"disconnected"})));
        let reader_status=Arc::clone(&status);
        let (send,receive)=mpsc::sync_channel(64);
        let replies=Arc::new(Mutex::new(HashMap::<String,mpsc::SyncSender<bool>>::new()));
        let answers=Arc::clone(&replies);
        std::thread::spawn(move || {
            let mut reader=BufReader::new(output);
            loop {
                let mut bytes=Vec::new();
                if reader.by_ref().take(1024*1024+1).read_until(b'\n',&mut bytes).unwrap_or(0)==0 { break; }
                if bytes.len()>1024*1024 || !bytes.ends_with(b"\n") { break; }
                let Ok(value)=serde_json::from_slice::<Value>(&bytes) else { break; };
                if value["type"]=="status" {
                    // Retain only owner-facing readiness, never document/grant IDs.
                    *reader_status.lock().unwrap()=json!({"link":value["link"],"attached":value["grant"].is_object()});
                    continue;
                }
                if value["type"]=="permission_result" {
                    if let Some(id)=value["id"].as_str() {
                        if let Some(sender)=answers.lock().unwrap().remove(id) {let _=sender.try_send(value["replied"]==true);}
                    }
                    continue;
                }
                if send.try_send(value).is_err() { break; }
            }
            answers.lock().unwrap_or_else(|e|e.into_inner()).clear();
            let mut child=reader_child.lock().unwrap_or_else(|e|e.into_inner());
            if reader_terminated.swap(true,std::sync::atomic::Ordering::SeqCst){return;}
            unsafe{libc::kill(-(child.id() as i32),libc::SIGKILL);}
            let _=child.kill();let _=child.wait();
        });
        Ok(Self{child,terminated,status,input:Mutex::new(input),output:Mutex::new(receive),permissions:Mutex::new(HashSet::new()),replies})
    }
    pub fn status(&self)->Value {
        let mut status=self.status.lock().unwrap().clone();
        status["running"]=json!(!self.terminated.load(std::sync::atomic::Ordering::SeqCst));
        status
    }
    pub fn send(&self,value:&Value)->Result<()> {
        let mut input=self.input.lock().unwrap();
        let data=format!("{}\n",value).into_bytes();
        if data.len()>65536 {return Err("Browser host control exceeds its limit".into());}
        let deadline=std::time::Instant::now()+Duration::from_secs(2);
        let mut offset=0;
        while offset<data.len() {
            match input.write(&data[offset..]) {
                Ok(0)=>{self.stop();return Err("Browser host closed input".into());},
                Ok(count)=>offset+=count,
                Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{
                    if std::time::Instant::now()>=deadline {self.stop();return Err("Browser host input timed out; host stopped".into());}
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) if e.kind()==std::io::ErrorKind::Interrupted=>continue,
                Err(e)=>{self.stop();return Err(e.to_string());},
            }
        }
        Ok(())
    }
    pub fn receive(&self)->Result<Option<Value>> {
        match self.output.lock().unwrap().recv_timeout(Duration::from_millis(50)) {
            Ok(v)=>Ok(Some(v)),Err(mpsc::RecvTimeoutError::Timeout)=>Ok(None),_=>Err("Browser host disconnected".into())
        }
    }
    pub fn permission(&self,id:&str) { self.permissions.lock().unwrap().insert(id.to_string()); }
    pub fn reply(&self,id:&str,decision:&str)->Result<Value> {self.reply_scoped(id,decision,false)}
    pub(crate) fn reply_scoped(&self,id:&str,decision:&str,persistent:bool)->Result<Value> {
        if !(matches!(decision,"allow"|"deny") || (persistent && decision=="always")) {return Err("Browser effects allow one-time allow or deny only".into());}
        if !self.permissions.lock().unwrap().remove(id) {return Err("Browser approval expired or already answered".into());}
        let (sender,receiver)=mpsc::sync_channel(1);
        self.replies.lock().unwrap().insert(id.to_string(),sender);
        if let Err(error)=self.send(&json!({"type":"permission_reply","id":id,"decision":decision})) {
            self.replies.lock().unwrap().remove(id);return Err(error);
        }
        let acknowledgement=receiver.recv_timeout(Duration::from_secs(2));
        self.replies.lock().unwrap().remove(id);
        let accepted=match acknowledgement {
            Ok(value)=>value,
            Err(_)=>{
                self.stop();
                return Err("Browser approval outcome unknown; host stopped. Do not retry this effect".into());
            }
        };
        if !accepted {return Err("Browser approval expired or was not accepted".into());}
        Ok(json!({"replied":true}))
    }
    pub fn clear(&self) { self.permissions.lock().unwrap().clear(); }
    pub fn stop(&self) {
        self.permissions.lock().unwrap_or_else(|e|e.into_inner()).clear();
        self.replies.lock().unwrap_or_else(|e|e.into_inner()).clear();
        let mut child=self.child.lock().unwrap_or_else(|e|e.into_inner());
        if self.terminated.swap(true,std::sync::atomic::Ordering::SeqCst){return;}
        unsafe{libc::kill(-(child.id() as i32),libc::SIGKILL);}
            let _=child.kill();let _=child.wait();
    }
}
impl Drop for BrowserHost {fn drop(&mut self){self.stop();}}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(body:&str)->(BrowserHost,std::path::PathBuf) {
        let path=std::env::temp_dir().join(format!("navi-host-{}.mjs",crate::id()));
        std::fs::write(&path,body).unwrap();
        (BrowserHost::start(&path).unwrap(),path)
    }
    #[test]
    fn permission_ack_is_one_shot_and_refusal_is_not_success() {
        let (host,path)=fixture(r#"import readline from 'node:readline';
        for await(const line of readline.createInterface({input:process.stdin})){
            const v=JSON.parse(line);console.log(JSON.stringify({type:'permission_result',id:v.id,replied:v.id.endsWith('yes')}));
        }"#);
        host.permission("navi-perm-yes");
        assert_eq!(host.reply("navi-perm-yes","always").is_err(),true);
        assert_eq!(host.reply("navi-perm-yes","allow").unwrap()["replied"],true);
        assert!(host.reply("navi-perm-yes","allow").is_err());
        host.permission("navi-perm-no");assert!(host.reply("navi-perm-no","allow").is_err());
        host.stop();let _=std::fs::remove_file(path);
    }
    #[test]
    fn malformed_or_oversized_output_disconnects_without_forwarding() {
        for source in ["console.log('not json')","console.log('x'.repeat(1024*1024+1))"] {
            let (host,path)=fixture(source);
            let deadline=std::time::Instant::now()+Duration::from_secs(2);
            loop {match host.receive(){Err(_)=>break,Ok(Some(_))=>panic!("unsafe output forwarded"),_=>{assert!(std::time::Instant::now()<deadline);}}}
            host.stop();let _=std::fs::remove_file(path);
        }
    }
    #[test]
    fn delayed_permission_cannot_execute_after_ack_timeout() {
        let (host,path)=fixture(r#"import readline from 'node:readline';
        for await(const line of readline.createInterface({input:process.stdin})){
            await new Promise(r=>setTimeout(r,2500));console.log(JSON.stringify({type:'permission_result',id:JSON.parse(line).id,replied:true}));
        }"#);
        host.permission("navi-perm-late");
        assert!(host.reply("navi-perm-late","allow").unwrap_err().contains("unknown"));
        assert!(host.child.lock().unwrap().try_wait().unwrap().is_some());
        let _=std::fs::remove_file(path);
    }
    #[test]
    fn output_burst_fails_closed_and_reaps_child() {
        let (host,path)=fixture("for(let i=0;i<200;i++)console.log(JSON.stringify({type:'delta',text:'x'}));setInterval(()=>{},1000)");
        std::thread::sleep(Duration::from_millis(200));
        assert!(host.child.lock().unwrap().try_wait().unwrap().is_some());
        host.stop();let _=std::fs::remove_file(path);
    }
    #[test]
    fn blocked_child_cannot_hold_stdin_forever() {
        let (host,path)=fixture("setInterval(()=>{},1000)");
        let value=json!({"type":"prompt","text":"x".repeat(60000)});
        let start=std::time::Instant::now();let mut failed=false;
        for _ in 0..4 {if host.send(&value).is_err(){failed=true;break;}}
        assert!(failed);assert!(start.elapsed()<Duration::from_secs(4));
        host.stop();let _=std::fs::remove_file(path);
    }
}
