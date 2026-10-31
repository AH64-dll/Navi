/* Native host transports structured RPC and a tmux attachment; no localhost web server. */
const send = message => window.webkit?.messageHandlers?.navi?.postMessage(JSON.stringify(message));
const element = id => document.getElementById(id);
let snapshot, active, seq = 0, requests = 0, motion = !matchMedia('(prefers-reduced-motion: reduce)').matches;
const pending = new Map();
const terminal = new Terminal({allowTransparency:true, cursorBlink:true, fontFamily:'"Nimbus Mono PS", monospace',fontSize:13,scrollback:10000,theme:{background:'#00000000',foreground:'#cee9ee',cursor:'#8dffe0',selectionBackground:'#698bd166'}});
terminal.open(element('terminal'));
terminal.onData(data => send({type:'input',data}));
terminal.onBinary(data => send({type:'binary',data:btoa(data)}));
function rpc(op, params = {}) {
  const ticket = ++requests;
  return new Promise((resolve,reject) => {pending.set(ticket,{resolve,reject}); send({type:'rpc',ticket,request:{op,params}});});
}
function toast(message) {
  element('toast').textContent=message; element('toast').classList.add('visible');
  clearTimeout(toast.timer); toast.timer=setTimeout(()=>element('toast').classList.remove('visible'),6000);
}
function fit() {
  const bounds=element('terminal').getBoundingClientRect();
  const probe=document.createElement('span'); probe.textContent='WWWWWWWWWW';probe.style.cssText='position:absolute;visibility:hidden;white-space:pre;font:13px "Nimbus Mono PS",monospace';document.body.append(probe);
  const width=probe.getBoundingClientRect().width/10;probe.remove();
  const cols=Math.max(20,Math.floor((bounds.width-18)/width));const rows=Math.max(6,Math.floor((bounds.height-8)/16));
  terminal.resize(cols,rows);send({type:'resize',cols,rows});
  window.naviScene?.setLayout(document.body.classList.contains('focus-view')?.5:.18,document.body.classList.contains('focus-view')?.7:0);
}
const resizeObserver=new ResizeObserver(()=>{clearTimeout(fit.timer);fit.timer=setTimeout(fit,80);});resizeObserver.observe(element('terminal'));
function update(value) {
  snapshot=value;
  const settings=value.settings;
  element('microphone').disabled=value.isolated;
  element('desktop').disabled=value.isolated;
  element('sessions').replaceChildren();
  for(const session of value.sessions) {
    const button=document.createElement('button');button.className='session'+(session.id===settings.active_session?' selected':'');button.disabled=!session.running;
    const dot=document.createElement('span');dot.className='dot'+(session.running?'':' dead');
    const label=document.createElement('span'),name=document.createElement('strong'),detail=document.createElement('small');name.textContent=session.name;detail.textContent=session.profile+' · '+(session.running?'running':'ended');label.append(name,detail);button.append(dot,label);
    button.onclick=()=>rpc('select',{id:session.id}).catch(error=>toast(error.message));element('sessions').append(button);
  }
  const current=value.sessions.find(session=>session.id===settings.active_session&&session.running);
  if(current?.id!==active) {
    active=current?.id;element('empty-session').hidden=Boolean(current);terminal.reset();
    send({type:'attach',id:active||null});element('session-name').textContent=current?.name||'Navi';element('harness-name').textContent=current?.profile||'Station';
  }
  element('microphone').setAttribute('aria-pressed',String(settings.microphone));element('microphone').firstElementChild.textContent=settings.microphone?'Mic on':'Mic off';
  element('chief-status').textContent=settings.model.name+' · '+value.voice.status;
  element('browser-status').textContent=value.browser_connected?'Connected to Zen':'Extension disconnected';
  element('desktop-status').textContent=value.desktop.status+(value.desktop.error?' · '+value.desktop.error:'');
  for(const event of value.events) {
    seq=Math.max(seq,event.seq);const line=document.createElement('div');line.textContent=new Date(event.time).toLocaleTimeString([], {hour:'2-digit',minute:'2-digit'})+' '+event.message;element('syslog').prepend(line);while(element('syslog').children.length>5)element('syslog').lastChild.remove();
    if(event.kind==='thinking'){element('state').textContent='deciding';window.naviScene?.setEnergy(.8);}
    if(event.kind==='reply')toast(event.details?.text||event.message);
    if(['command','error','stopped'].includes(event.kind)){element('state').textContent=event.kind==='error'?'attention':'ready';window.naviScene?.setEnergy(.1);if(event.kind==='error')toast(event.message);}
  }
}
async function poll() {
  try {update(await rpc('status',{gui:true,since:seq}));}catch(error){toast(error.message);}
  setTimeout(poll,700);
}
function launch() {
  if(!snapshot)return;
  element('launch-profile').replaceChildren();for(const profile of snapshot.settings.profiles){const option=document.createElement('option');option.value=profile.id;option.textContent=profile.name;element('launch-profile').append(option);}
  element('launch-cwd').value=snapshot.settings.projects[0]?.path||'';element('launch-name').value='';element('launch-dialog').showModal();
}
element('new-session').onclick=launch;element('empty-launch').onclick=launch;
element('launch-form').onsubmit=async event=>{event.preventDefault();try{await rpc('create',{profile:element('launch-profile').value,name:element('launch-name').value||undefined,cwd:element('launch-cwd').value});element('launch-dialog').close();}catch(error){toast(error.message);}};
element('command-form').onsubmit=async event=>{event.preventDefault();const text=element('command').value.trim();if(!text)return;element('command').value='';try{const result=await rpc('command',{text});const reply=result.reply?.text||result.reply;const message=reply||result.trace.at(-1)?.result?.message;if(message)toast(message);}catch(error){toast(error.message);}};
element('stop').onclick=()=>rpc('cancel').catch(error=>toast(error.message));
element('microphone').onclick=async()=>{if(snapshot){try{await rpc('settings',{...snapshot.settings,microphone:!snapshot.settings.microphone});await rpc('voice_start');}catch(error){toast(error.message);}}};
element('desktop').onclick=()=>rpc(snapshot?.desktop.status==='connected'?'desktop_stop':'desktop_start').then(()=>toast(snapshot?.desktop.status==='connected'?'Desktop disconnected':'Choose the target in the desktop consent dialog')).catch(error=>toast(error.message));
element('settings-button').onclick=()=>{if(!snapshot)return;const settings=snapshot.settings;for(const key of ['profiles','projects','favorites'])element(key).value=JSON.stringify(settings[key],null,2);element('speech').value=JSON.stringify(settings.speech||{engine:'whistle',wake_score:1,wake_threshold:.25},null,2);for(const key of ['name','endpoint','server','weights','projection'])element('model-'+key).value=settings.model[key]||'';element('model-gpu').value=settings.model.gpu_layers;element('model-threads').value=settings.model.threads||2;element('model-confidence').value=settings.model.minimum_probability;element('listening').value=settings.listening;element('persona').value=settings.persona||'jarvis';element('settings-dialog').showModal();};
element('settings-form').onsubmit=async event=>{event.preventDefault();try{const model={...snapshot.settings.model,gpu_layers:Number(element('model-gpu').value),threads:Number(element('model-threads').value),minimum_probability:Number(element('model-confidence').value)};for(const key of ['name','endpoint','server','weights','projection'])model[key]=element('model-'+key).value;const settings={...snapshot.settings,profiles:JSON.parse(element('profiles').value),projects:JSON.parse(element('projects').value),favorites:JSON.parse(element('favorites').value),speech:JSON.parse(element('speech').value),listening:element('listening').value,persona:element('persona').value,model};await rpc('settings',settings);element('settings-dialog').close();toast('Settings saved');}catch(error){toast(error.message);}};
document.querySelectorAll('[data-close]').forEach(button=>{button.onclick=()=>element(button.dataset.close).close();});
element('browser-folder').onclick=()=>send({type:'extension-folder'});
function focusView(){document.body.classList.toggle('focus-view');setTimeout(fit,750);}
element('focus-top').onclick=focusView;
function setMotion(){window.naviScene?.setMotion(motion);document.body.classList.toggle('still',!motion);element('motion-button').setAttribute('aria-pressed',String(motion));}
element('motion-button').onclick=()=>{motion=!motion;setMotion();};element('fullscreen-button').onclick=()=>send({type:'fullscreen'});
addEventListener('keydown',event=>{const shortcuts=snapshot?.settings.shortcuts;if(event.key===shortcuts?.focus){event.preventDefault();focusView();}if(event.key===shortcuts?.fullscreen){event.preventDefault();send({type:'fullscreen'});}});
window.navi={
  reply(ticket,reply){const request=pending.get(ticket);if(request){pending.delete(ticket);if(reply.ok)request.resolve(reply.result);else request.reject(new Error(reply.error));}},
  receive(encoded){terminal.write(Uint8Array.from(atob(encoded),c=>c.charCodeAt(0)));},
  notice:toast,
  inspect(){return {renderer:element('field').dataset.renderer,sessions:snapshot?.sessions,active,terminal:terminal.buffer.active.getLine(0)?.translateToString(),title:element('session-name').textContent};}
};
window.naviScene?.boot(()=>{document.body.classList.remove('booting');document.body.classList.add('booted');setMotion();fit();},true);
send({type:'ready'});poll();
