/* Native host transports structured RPC; the UI is the stage, a session strip,
 * one chat panel, and an attached harness terminal. */
/* naviHost is injected by window.py at document start: host flags that the page
 * cannot read from the environment. Test hooks appear only in isolated mode. */
const host = window.naviHost ?? {};
const send = message => {
  const bridge = window.webkit?.messageHandlers?.navi;
  if (!bridge) return false;
  try { bridge.postMessage(JSON.stringify(message)); } catch { return false; }
  return true;
};
const element = id => document.getElementById(id);
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
let snapshot, seq = 0, requests = 0, motion = !reducedMotion.matches;
const pending = new Map();
let busy = false;
/* Events older than this are history the feed replays (it starts from seq 0 on
 * every boot and after a daemon restart); narrating rows are drawn for live
 * events only. */
const REPLAY_SLACK=15000;
let replayBefore=Date.now()-REPLAY_SLACK;
/* Dialogs open only for replies newer than this page: the replay slack above keeps
 * an in-flight answer live across a reload, but must not reopen a dialog. */
const PAGE_START=Date.now();

/* ---------------- transport: bounded, expiring, self-healing ---------------- */

/* A reply is not guaranteed: the daemon can die, the host can lose the page, or
 * a message can be malformed. Every request carries a delivery budget and is
 * rejected by one sweeper instead of parking its promise forever. */
const budgets={status:15000,ping:10000,window_state:10000,settings:20000,create:20000,
  command:200000,prompt:20000,cancel:10000,select:10000,attach:10000,
  voice_start:20000,voice_stop:20000,desktop_start:20000,desktop_stop:20000,
  oc_models:30000,oc_agents:30000,oc_commands:30000,oc_new:30000};
const DEFAULT_BUDGET=15000, PENDING_LIMIT=64, SWEEP_INTERVAL=5000, DIAGNOSTIC_LIMIT=200;
const diagnostics=[];
let timeouts=0, dropped=0, unknownReplies=0;
const rpcLog=[];   // isolated-only: the operations this page actually sent
const rpcCalls=[]; // isolated-only: every sent call with its params

function record(level, message, stack) {
  diagnostics.push({type:'log',at:new Date().toISOString(),level,message:String(message ?? ''),
    ...(stack ? {stack:String(stack)} : {})});
  while (diagnostics.length > DIAGNOSTIC_LIMIT) diagnostics.shift();
  /* With NAVI_DEVTOOLS the host forwards console output to the terminal, so a
   * failure is readable without attaching to the inspector. */
  if (level === 'error' && host.devtools) console.error('[navi] ' + message + (stack ? '\n' + stack : ''));
}
function timeoutError(op, budget) {
  const error=new Error(`timeout: ${op} got no reply within ${budget}ms`);
  error.name='TimeoutError'; error.code='timeout'; error.op=op; error.budget=budget;
  return error;
}
function rpc(op, params = {}, timeout = budgets[op] ?? DEFAULT_BUDGET) {
  const ticket = ++requests;
  /* Isolated-only: the workflow asserts which operations the UI actually sent
   * (a permission decision, a model save) without depending on the daemon's reply. */
  if (host.isolated===true) {
    rpcLog.push(op);
    while (rpcLog.length>200) rpcLog.shift();
    rpcCalls.push({op,params});
    while (rpcCalls.length>200) rpcCalls.shift();
  }
  /* Cap the map instead of growing without limit. The oldest entry is the most
   * likely to be dead, so it loses; refusing fresh work would stall the UI. */
  if (pending.size >= PENDING_LIMIT) {
    const [oldest, entry] = pending.entries().next().value;
    pending.delete(oldest);
    record('warn', `pending overflow: evicted ${entry.op} ticket ${oldest}`);
    entry.reject(timeoutError(entry.op, entry.budget));
  }
  return new Promise((resolve, reject) => {
    const budget=Math.max(1, timeout);
    pending.set(ticket, {op, resolve, reject, budget, expires:Date.now() + budget});
    if (!send({type:'rpc', ticket, request:{op, params}})) {
      pending.delete(ticket);
      reject(new Error('Navi host is not connected'));
    }
  });
}
function sweep() {
  const now=Date.now();
  for (const [ticket, entry] of [...pending]) {
    if (now < entry.expires) continue;
    pending.delete(ticket);
    timeouts++;
    const error=timeoutError(entry.op, entry.budget);
    record('error', error.message);
    entry.reject(error);
  }
}
setInterval(sweep, SWEEP_INTERVAL);
function toast(message) {
  element('toast').textContent=message; element('toast').classList.add('visible');
  clearTimeout(toast.timer); toast.timer=setTimeout(()=>element('toast').classList.remove('visible'),6000);
}
/* The clipboard API is not available on every file:// page, so the diagnostics
 * path (a hidden textarea and execCommand) stays as the fallback. */
async function copyText(value) {
  const text=String(value??'');
  try {
    if (navigator.clipboard&&navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch { /* fall through to the deprecated path */ }
  const area=document.createElement('textarea');
  area.value=text; area.setAttribute('readonly',''); area.style.cssText='position:fixed;top:-1000px';
  document.body.append(area);
  area.select();
  let copied=false;
  try { copied=document.execCommand('copy'); } catch (error) { record('warn','copy failed: '+error.message); }
  area.remove();
  return copied;
}

/* ---------------- chat: the message model ----------------
 * A message is data — `{id,turn,role,time,status,parts[],meta}` — and every row is a
 * projection of it. Rows used to be text appends on a volatile event ring, so a
 * stream could never be finalised in place, a repeated answer was suppressed by a
 * 15 s text window, a failed turn left no trace at all, and nothing could be saved
 * (BUG-037/045/046/047/048). Parts are ordered and typed: text, reasoning, tool,
 * diff, error. Nothing here reads or writes innerHTML. */

const RENDER_WINDOW=50;        // rows mounted either side of the viewport (±50)
const ROW_ESTIMATE=44;         // px for a row that has never been measured
const TOOL_TEXT_LIMIT=4000;    // a longer tool output is truncated in the row
const STORE_TEXT_LIMIT=8000;   // a longer tool output is truncated in the database
const SEEN_LIMIT=500;          // event seqs remembered for de-duplication
const HISTORY_START=200, HISTORY_MAX=500;   // messages() caps at 500 server-side

const messages=[];             // truth, oldest first; never capped or truncated
const messageById=new Map();
const rowHeights=new Map();    // id -> last measured height in px
const mountedRows=new Map();   // id -> {row, version}
const seenSeqs=new Set(), seenOrder=[];
const failures=new Map();      // failure text -> when it was last shown
let mountedOrder=[], spacerTop=0, spacerBottom=0;
let turn=null;                 // the open turn: {message,user,started}
let request=null;              // the in-flight submit: {text,user,done,queued}
let activeText='';             // the prompt of the open turn, for Retry
let turnLabel='', needsAttention=false, chatFrame=0;
let identCount=0, historyLimit=HISTORY_START, pagingHistory=false;
/* The active project is client state, not a setting (BUG-090): it belongs to
 * this window, survives a reload in localStorage, and never rewrites the
 * daemon's settings. A project-scoped action reads it; an event's own
 * `project_id` never changes it. */
const PROJECT_KEY='navi.project';
let activeProject=readProject();
function readProject() {
  try {
    const value=localStorage.getItem(PROJECT_KEY);
    return typeof value==='string'&&value?value:null;
  } catch { return null; }
}
function writeProject(id) {
  try { localStorage.setItem(PROJECT_KEY,id); }
  catch { /* storage can be unavailable; the choice still holds for this session */ }
}
/* Pending attachments (BUG-005): at most four, 2 MiB decoded each, read as base64
 * and handed to the daemon with the prompt. A rejected file is visible at once;
 * nothing is silently dropped. */
const ATTACH_MAX=4, ATTACH_BYTES=2*1024*1024, ATTACH_NAME_MAX=120;
let attachments=[];
let lastCommand=null;
/* Paging state: the oldest sqlite cursor this page holds, and whether the store
 * still has rows before it (BUG-047). */
let oldestCursor=null, historyHasMore=false;
const chatLog=()=>element('chat-log');

function uid(prefix){ identCount+=1; return prefix+'-'+Date.now().toString(36)+'-'+identCount.toString(36); }
function clock(ms) {
  const date=new Date(Number(ms)||Date.now());
  return String(date.getHours()).padStart(2,'0')+':'+String(date.getMinutes()).padStart(2,'0');
}
function truncateText(value,limit) {
  const text=String(value??'');
  return text.length>limit?text.slice(0,limit)+'\n… truncated':text;
}
function fieldText(value) { return typeof value==='string'?value:JSON.stringify(value,null,2); }
function sameList(a,b) {
  if (a.length!==b.length) return false;
  for (let index=0;index<a.length;index+=1) if (a[index]!==b[index]) return false;
  return true;
}
function isModelRef(value) { return typeof value==='string'&&/^[^/\s]+\/\S+$/.test(value); }
function modelRef() { return String(snapshot?.settings?.assistant?.model||''); }
/* "commandcode/deepseek/deepseek-v4.1-flash" is a stored reference, not a name: the
 * status line shows the short form and never a path with slashes (BUG-035). */
function shortModel(reference) {
  const value=String(reference||'').trim();
  if (!value) return '';
  const tail=value.includes('/')?value.slice(value.lastIndexOf('/')+1):value;
  return tail.replace(/[_-]+/g,' ').replace(/\s+/g,' ').trim()
    .replace(/\b[a-z]/g,letter=>letter.toUpperCase());
}
function formatDuration(ms) {
  if (typeof ms!=='number'||!isFinite(ms)||ms<0) return '';
  return ms<1000?Math.round(ms)+'ms':(ms/1000).toFixed(1)+'s';
}
function usageLine(usage) {
  if (!usage||typeof usage!=='object') return '';
  const bits=[];
  const tokens=usage.tokens;
  let total=null;
  if (typeof tokens==='number') total=tokens;
  else if (tokens&&typeof tokens==='object') total=Object.values(tokens).reduce((sum,value)=>sum+(Number(value)||0),0);
  if (total!==null&&total>0) bits.push(total>=1000?(total/1000).toFixed(1)+'k tok':total+' tok');
  if (typeof usage.cost==='number'&&usage.cost>0) bits.push(usage.cost<0.01?'$'+usage.cost.toFixed(4):'$'+usage.cost.toFixed(3));
  if (typeof usage.finish==='string'&&usage.finish) bits.push(usage.finish);
  return bits.join(' · ');
}

/* -- model ---------------------------------------------------------------- */

function addMessage(fields) {
  const message={
    id:fields.id||uid('m'),
    cursor:Number.isFinite(Number(fields.cursor))?Number(fields.cursor):null,
    turn:fields.turn??null,
    role:['user','assistant','system'].includes(fields.role)?fields.role:'assistant',
    time:Number(fields.time)||Date.now(),
    status:fields.status||'complete',
    parts:Array.isArray(fields.parts)?fields.parts:[],
    meta:fields.meta||{},
    version:0,
    index:messages.length,
  };
  messages.push(message);
  messageById.set(message.id,message);
  chatChanged();
  return message;
}

/* Hydration merges by id and keeps the log in time order; a message already on
 * screen is never re-inserted, which is what makes a rehydrate idempotent.
 * `older` pages arrive in cursor order and are strictly before everything already
 * loaded, so they are prepended as a block: a shared timestamp can never reorder
 * the transcript, and the two pages meet at a cursor rather than a time (BUG-047). */
function hydrateMessages(rows, options={}) {
  const fresh=[];
  for (const row of rows||[]) {
    const id=typeof row?.id==='string'?row.id:null;
    if (!id||messageById.has(id)) continue;
    const role=['user','assistant','system','tool'].includes(row.role)?row.role:'assistant';
    fresh.push({
      id, turn:row.turn??null,
      cursor:Number.isFinite(Number(row.cursor))?Number(row.cursor):null,
      role:role==='tool'?'system':role,
      time:Number(row.time)||Date.now(),
      status:'complete',
      parts:Array.isArray(row.parts)?row.parts:[],
      meta:{project_id:row.project_id||undefined},
      version:0, index:0,
    });
  }
  if (!fresh.length) return 0;
  /* A stored failure comes back as a failure: its row keeps the failed status,
   * and its Retry re-sends the request it answered (the user row before it). */
  let asked=null;
  for (const message of fresh) {
    if (message.role==='user') { asked=message.parts.find(part=>part.type==='text')?.text||null; continue; }
    if (!message.parts.some(part=>part.type==='error')) continue;
    message.status='failed';
    if (asked) message.meta.request=String(asked);
  }
  if (options.older===true) {
    messages.unshift(...fresh);
    for (const message of fresh) messageById.set(message.id,message);
  } else {
    for (const message of fresh) {
      let position=messages.length;
      while (position>0&&messages[position-1].time>message.time) position-=1;
      messages.splice(position,0,message);
      messageById.set(message.id,message);
    }
  }
  reindex();
  chatChanged();
  return fresh.length;
}
function reindex() { messages.forEach((message,index)=>{ message.index=index; }); }
/* Remove a row that never received content (a turn placeholder). */
function dropEmptyRow(id) {
  const message=messageById.get(id);
  if (!message||messageText(message)) return;
  const index=messages.indexOf(message);
  if (index>=0) messages.splice(index,1);
  messageById.delete(id);
  reindex();
  chatChanged();
}
function chatChanged() {
  updateEmptyState();
  if (!chatFrame) chatFrame=requestAnimationFrame(paintChat);
}
function bump(message) {
  if (!message) return;
  message.version+=1;
  chatChanged();
}
function lastTextPart(message,create) {
  const tail=message.parts.at(-1);
  if (tail&&tail.type==='text') return tail;
  if (!create) return null;
  const part={type:'text',text:''};
  message.parts.push(part);
  return part;
}
function messageText(message) {
  return message.parts.filter(part=>part.type==='text').map(part=>part.text||'').join('\n').trim();
}
function updateEmptyState() {
  const empty=element('chat-empty');
  if (!empty) return;
  const conversation=messages.some(message=>message.role==='user'||message.role==='assistant'
    ||(message.role==='system'&&(message.status==='failed'
      ||message.parts.some(part=>part.tone==='progress'))));
  empty.hidden=conversation;
}

/* -- rows and parts ------------------------------------------------------- */

const SVG_NS='http://www.w3.org/2000/svg';
function icon(path,size=12) {
  const node=document.createElementNS(SVG_NS,'svg');
  node.setAttribute('viewBox','0 0 24 24'); node.setAttribute('width',size); node.setAttribute('height',size);
  node.setAttribute('aria-hidden','true');
  const shape=document.createElementNS(SVG_NS,'path');
  shape.setAttribute('d',path); shape.setAttribute('fill','currentColor');
  node.append(shape);
  return node;
}
const COPY_ICON='M16 1H4a2 2 0 0 0-2 2v14h2V3h12zm3 4H8a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h11a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2zm0 16H8V7h11z';
const RETRY_ICON='M12 5V1L7 6l5 5V7a6 6 0 1 1-6 6H4a8 8 0 1 0 8-8z';
const EDIT_ICON='M3 17.25V21h3.75L17.8 9.94l-3.75-3.75L3 17.25zM20.7 7.04a1 1 0 0 0 0-1.41l-2.34-2.34a1 1 0 0 0-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z';

/* A row always exists; text is a part inside it. The old factory refused an empty
 * body, which is exactly why the streaming path was unreachable (BUG-048). */
function row(role='assistant') {
  const node=document.createElement('article');
  node.className='msg '+(role==='assistant'?'navi':role);
  node.dataset.role=role;
  node.tabIndex=0;
  const head=document.createElement('header'); head.className='msg-head';
  const who=document.createElement('span'); who.className='who';
  const time=document.createElement('span'); time.className='msg-time';
  head.append(who,time);
  const body=document.createElement('div'); body.className='msg-body';
  const meta=document.createElement('div'); meta.className='msg-meta'; meta.hidden=true;
  const actions=document.createElement('div'); actions.className='msg-actions';
  node.append(head,body,meta,actions);
  return node;
}

function createRow(message) {
  const node=row(message.role);
  node.dataset.id=message.id;
  return node;
}

function whoLabel(message) {
  if (message.role==='user') return 'You';
  if (message.role==='system') return 'System';
  return 'Navi';
}
function metaLine(message) {
  const meta=message.meta||{};
  const bits=[];
  if (meta.model) {
    const actual=shortModel(meta.model);
    const requested=shortModel(modelRef());
    const requestedFull=String(meta.requested||modelRef()||'');
    bits.push(actual&&requestedFull&&meta.model!==requestedFull?actual+' (requested: '+requested+')':actual);
  }
  if (typeof meta.durationMs==='number') bits.push(formatDuration(meta.durationMs));
  const usage=usageLine(meta.usage);
  if (usage) bits.push(usage);
  if (meta.agent) bits.push(meta.agent);
  if (meta.session) bits.push('session '+String(meta.session).slice(0,8));
  /* The source is internal routing ("navi", "unavailable", "codex", "detector")
   * that the row's own text already states; only a spoken request says
   * something the row does not. */
  if (meta.source==='voice') bits.push('spoken');
  return bits.filter(Boolean).join(' · ');
}

function renderRow(node,message) {
  node.className='msg '+(message.role==='assistant'?'navi':message.role)+(message.status==='streaming'?' streaming':'');
  node.dataset.id=message.id;
  node.dataset.status=message.status;
  node.dataset.role=message.role;
  const head=node.querySelector('.msg-head');
  head.querySelector('.who').textContent=whoLabel(message);
  const time=head.querySelector('.msg-time');
  time.textContent=clock(message.time);
  time.title=new Date(message.time).toLocaleString();
  const body=node.querySelector('.msg-body');
  body.replaceChildren(...message.parts.map(part=>renderPart(message,part)));
  if (choiceOffer&&choiceOffer.message===message) body.append(choiceButtons(choiceOffer.choices));
  const meta=node.querySelector('.msg-meta');
  const line=metaLine(message);
  meta.hidden=!line;
  meta.textContent=line;
  buildActions(node,message);
}

/* "Which one: Codex #1, Codex #2?": the daemon holds the question for a minute and
 * any next command settles it, so its choices are buttons on that row until then.
 * They live outside the message, so a reload never shows a stale question. */
let choiceOffer=null;
function offerChoices(message,choices,request) {
  clearChoices();
  choiceOffer={message,request,choices:choices.slice(0,8).map(String),timer:setTimeout(clearChoices,60000)};
  bump(message);
}
function clearChoices() {
  if (!choiceOffer) return;
  const {message,timer}=choiceOffer;
  choiceOffer=null;
  clearTimeout(timer);
  bump(message);
}
function choiceButtons(choices) {
  const row=document.createElement('div');
  row.className='msg-choices';
  for (const label of choices) {
    const button=document.createElement('button');
    button.type='button'; button.className='msg-choice'; button.textContent=label;
    button.onclick=event=>{ event.preventDefault(); dispatch(label); };
    row.append(button);
  }
  return row;
}

function action(name,label,path,handler) {
  const button=document.createElement('button');
  button.type='button';
  button.className='msg-action';
  button.dataset.action=name;
  if (path) { button.append(icon(path)); const text=document.createElement('span'); text.textContent=label; button.append(text); button.title=label; button.setAttribute('aria-label',label); }
  else button.textContent=label;
  button.onclick=event=>{ event.preventDefault(); event.stopPropagation(); handler(); };
  return button;
}

function buildActions(node,message) {
  const bar=node.querySelector('.msg-actions');
  bar.replaceChildren();
  if (message.role==='system') return;
  const text=messageText(message);
  if (text) bar.append(action('copy','Copy',COPY_ICON,()=>copyMessage(message)));
  if (message.role==='user') bar.append(action('edit','Edit and resend',EDIT_ICON,()=>editMessage(message)));
  if (message.role==='assistant'&&message.meta?.request&&!message.parts.some(part=>part.type==='error')) {
    bar.append(action('retry','Retry',RETRY_ICON,()=>retryMessage(message)));
  }
}

function copyMessage(message) {
  copyText(messageText(message)).then(ok=>toast(ok?'Message copied':'Could not copy the message'));
}
function editMessage(message) {
  const input=commandInput();
  if (!input) return;
  input.value=messageText(message);
  input.focus();
  input.setSelectionRange(input.value.length,input.value.length);
  toast('Edit the message, then press Enter to resend it');
}
function retryMessage(message) {
  const text=String(message.meta?.request||'').trim();
  if (!text) { toast('There is nothing to retry for this message'); return; }
  dispatch(text,{source:'retry'});
}

/* -- markdown: DOM only, never innerHTML ---------------------------------- */

const INLINE=/`([^`\n]+)`|\*\*([^*\n]+)\*\*|__([^_\n]+)__|\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)|(https?:\/\/[^\s<>"'`]+)/g;

function linkButton(url,label) {
  /* No navigation: the host blocks every non-local URI, and CSP has no network.
   * The link is text the user can copy, which is the honest affordance. */
  const node=document.createElement('button');
  node.type='button';
  node.className='md-link';
  node.textContent=label||url;
  node.title=url+' — click to copy this link';
  node.onclick=event=>{
    event.preventDefault();
    copyText(url).then(ok=>toast(ok?'Link copied':'Could not copy the link'));
  };
  return node;
}

function renderInline(node,text) {
  const value=String(text??'');
  let index=0;
  INLINE.lastIndex=0;
  for (let match=INLINE.exec(value);match;match=INLINE.exec(value)) {
    if (match.index>index) node.append(document.createTextNode(value.slice(index,match.index)));
    if (match[1]!==undefined) {
      const code=document.createElement('code'); code.className='md-code'; code.textContent=match[1]; node.append(code);
    } else if (match[2]!==undefined||match[3]!==undefined) {
      const strong=document.createElement('strong'); strong.textContent=match[2]??match[3]; node.append(strong);
    } else if (match[4]!==undefined) {
      node.append(linkButton(match[5],match[4]));
    } else {
      node.append(linkButton(match[5],match[5]));
    }
    index=match.index+match[0].length;
  }
  if (index<value.length) node.append(document.createTextNode(value.slice(index)));
}

function looksLikeDiff(value) {
  const text=String(value??'');
  if (!text) return false;
  if (/^diff --git /m.test(text)) return true;
  if (/^--- \S+/m.test(text)&&/^\+\+\+ \S+/m.test(text)) return true;
  return /^@@ .* @@/m.test(text)&&/^[+-]/m.test(text);
}
function renderDiffLines(code,text) {
  const lines=String(text??'').split('\n');
  lines.forEach((line,index)=>{
    const span=document.createElement('span');
    let tone='';
    if (/^(\+\+\+|---)/.test(line)) tone=' file';
    else if (/^@@/.test(line)) tone=' hunk';
    else if (/^\+/.test(line)) tone=' add';
    else if (/^-/.test(line)) tone=' del';
    span.className='diff-line'+tone;
    span.textContent=line+(index<lines.length-1?'\n':'');
    code.append(span);
  });
}

function codeBlock(lang,text) {
  const wrap=document.createElement('div'); wrap.className='code';
  const head=document.createElement('div'); head.className='code-head';
  const tag=document.createElement('span'); tag.className='code-lang'; tag.textContent=lang||'text';
  const copy=document.createElement('button'); copy.type='button'; copy.className='code-copy'; copy.textContent='Copy';
  copy.title='Copy this block';
  copy.onclick=event=>{
    event.preventDefault();
    copyText(text).then(ok=>{ copy.textContent=ok?'Copied':'Failed'; setTimeout(()=>{ copy.textContent='Copy'; },1600); });
  };
  head.append(tag,copy);
  const pre=document.createElement('pre');
  const code=document.createElement('code');
  code.className='lang-'+(lang||'text').replace(/[^a-z0-9+#-]/gi,'');
  if ((lang||'').toLowerCase()==='diff'||looksLikeDiff(text)) renderDiffLines(code,text);
  else code.textContent=text;
  pre.append(code);
  wrap.append(head,pre);
  return wrap;
}

function renderMarkdown(container,text) {
  const lines=String(text??'').split('\n');
  let paragraph=[], list=null, listTag=null;
  const flushParagraph=()=>{
    if (!paragraph.length) return;
    const node=document.createElement('p');
    renderInline(node,paragraph.join('\n'));
    container.append(node);
    paragraph=[];
  };
  const closeList=()=>{ list=null; listTag=null; };
  for (let index=0;index<lines.length;index+=1) {
    const line=lines[index];
    const fence=/^\s*(`{3,}|~{3,})\s*([\w.+#-]*)\s*$/.exec(line);
    if (fence) {
      flushParagraph(); closeList();
      const marker=fence[1][0]==='`'?'`':'~';
      const closing=new RegExp('^\\s*\\'+marker+'{3,}\\s*$');
      const body=[];
      for (index+=1;index<lines.length;index+=1) {
        if (closing.test(lines[index])) break;
        body.push(lines[index]);
      }
      container.append(codeBlock(fence[2]||'',body.join('\n')));
      continue;
    }
    if (!line.trim()) { flushParagraph(); closeList(); continue; }
    const progress=progressLine(line);
    if (progress) {
      flushParagraph(); closeList();
      let group=container.lastElementChild;
      if (!group||!group.classList.contains('progress-chips')) {
        group=document.createElement('div');
        group.className='progress-chips';
        container.append(group);
      }
      group.append(progressChip(progress));
      continue;
    }
    const bullet=/^\s*[-*+]\s+(.*)$/.exec(line);
    const numbered=/^\s*\d{1,9}[.)]\s+(.*)$/.exec(line);
    if (bullet||numbered) {
      flushParagraph();
      const tag=bullet?'ul':'ol';
      if (!list||listTag!==tag) { list=document.createElement(tag); list.className='md-list'; listTag=tag; container.append(list); }
      const item=document.createElement('li');
      renderInline(item,(bullet||numbered)[1]);
      list.append(item);
      continue;
    }
    const heading=/^\s*(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flushParagraph(); closeList();
      const node=document.createElement(heading[1].length<=2?'h3':'h4');
      renderInline(node,heading[2]);
      container.append(node);
      continue;
    }
    closeList();
    paragraph.push(line);
  }
  flushParagraph();
}

/* The assistant's machine-readable progress lines (recorded by the daemon) read
 * as chips: the grammar mirrors detector::parse_progress_lines. */
function progressLine(line) {
  const match=/^[\s>*`-]*(?:PROGRESS|Progress:|progress)\s*:?\s+(.*)$/.exec(line);
  if (!match) return null;
  let rest=match[1].replace(/`+\s*$/,''), evidence='';
  const bar=rest.indexOf('|');
  if (bar>=0) { evidence=rest.slice(bar+1).trim(); rest=rest.slice(0,bar).trim(); }
  let part=null, tail='';
  const overall=/^(?:overall|whole project|project|total)\b(.*)$/i.exec(rest);
  if (overall) tail=overall[1];
  else {
    const named=/^(?:part|aspect|area)\b\s*(.*)$/i.exec(rest);
    if (!named) return null;
    const body=named[1].trim();
    const quoted=/^["“]([^"”]*)["”](.*)$/.exec(body);
    if (quoted) { part=quoted[1].trim(); tail=quoted[2]; }
    else {
      const split=body.lastIndexOf(' ');
      if (split<0) return null;
      part=body.slice(0,split).trim().replace(/:$/,'');
      tail=body.slice(split);
    }
    if (!part) return null;
  }
  const number=/^[\s:=]*(\d+(?:\.\d+)?)/.exec(tail);
  if (!number) return null;
  const percent=Number(number[1]);
  if (!Number.isFinite(percent)||percent<0||percent>100) return null;
  return {part,percent,evidence};
}
function progressChip(entry) {
  const chip=document.createElement('span');
  chip.className='progress-chip';
  chip.dataset.scope=entry.part?'part':'overall';
  const name=document.createElement('span');
  name.className='chip-name';
  name.textContent=entry.part||'Overall';
  const value=document.createElement('b');
  value.className='chip-value';
  value.textContent=Math.round(entry.percent)+'%';
  const bar=document.createElement('i');
  bar.className='chip-bar';
  bar.style.setProperty('--fill',Math.round(entry.percent)+'%');
  chip.append(name,value,bar);
  chip.title=entry.evidence||'Recorded as an estimate';
  return chip;
}

function textPart(part) {
  const box=document.createElement('div');
  box.className='part text'+(part.tone?' tone-'+part.tone:'');
  renderMarkdown(box,part.text||'');
  return box;
}

function reasoningPart(part) {
  const box=document.createElement('details');
  box.className='part reasoning';
  const summary=document.createElement('summary');
  summary.textContent='Reasoning';
  const body=document.createElement('div');
  body.className='reasoning-body';
  renderMarkdown(body,part.text||'');
  box.append(summary,body);
  return box;
}

function toolState(part) {
  if (part.approval&&!part.approval.decision) return 'needs approval';
  if (part.approval&&part.approval.decision) return 'approved: '+part.approval.decision;
  const exit=part.exit;
  const suffix=exit===null||exit===undefined||exit===''?'':' · exit '+exit;
  return (part.status||'running')+suffix;
}
function toolBlock(label,value) {
  const block=document.createElement('div');
  block.className='tool-block';
  const heading=document.createElement('span');
  heading.className='tool-label'; heading.textContent=label;
  const pre=document.createElement('pre');
  pre.textContent=truncateText(fieldText(value),TOOL_TEXT_LIMIT);
  block.append(heading,pre);
  return block;
}
function diffPart(part) {
  const wrap=document.createElement('div');
  wrap.className='part diff';
  if (part.path) {
    const head=document.createElement('div'); head.className='diff-head'; head.textContent=part.path;
    wrap.append(head);
  }
  const pre=document.createElement('pre'), code=document.createElement('code');
  renderDiffLines(code,part.text||'');
  pre.append(code); wrap.append(pre);
  return wrap;
}
function approvalRow(message,part) {
  const box=document.createElement('div');
  box.className='approval';
  const label=document.createElement('span');
  label.className='approval-label';
  label.textContent=part.approval.decision
    ? 'Decision: '+part.approval.decision
    : (part.approval.tool||'This tool')+' needs approval';
  box.append(label);
  if (!part.approval.decision) {
    for (const decision of ['allow','deny']) {
      const button=document.createElement('button');
      button.type='button';
      button.className='approval-'+decision;
      button.textContent=decision==='allow'?'Allow':'Deny';
      button.onclick=event=>{
        event.preventDefault();
        answerApproval(message,part,decision);
      };
      box.append(button);
    }
  }
  return box;
}
function answerApproval(message,part,decision) {
  const id=part.approval.id;
  /* BUG-006: the daemon answers `{ok:false,error}` when it refuses the decision,
   * and a transport failure rejects; either way the refusal is one failure surface
   * with the daemon's own text — never a silent success, and never a second row. */
  rpc('permission_reply',{id,decision}).then(result=>{
    const refused=result&&(result.ok===false||result.replied===false);
    if (refused) {
      surfaceFailure(String(result.error||'The permission reply was not accepted'));
      return;
    }
    part.approval.decision=decision;
    part.status='decided';
    bump(message);
  }).catch(error=>surfaceFailure(error.message));
}
function toolPart(message,part) {
  const box=document.createElement('details');
  box.className='part tool';
  box.dataset.tool=part.id||'';
  box.dataset.status=part.approval&&!part.approval.decision?'approval':(part.status||'running');
  if (part.final) box.dataset.final='true';
  const summary=document.createElement('summary');
  const name=document.createElement('span'); name.className='tool-name'; name.textContent=part.name||'tool';
  const state=document.createElement('span'); state.className='tool-state'; state.textContent=toolState(part);
  summary.append(name,state);
  box.append(summary);
  const body=document.createElement('div');
  body.className='tool-body';
  if (part.input!==undefined&&part.input!==null&&part.input!=='') body.append(toolBlock('input',part.input));
  if (part.output!==undefined&&part.output!==null&&part.output!=='') {
    if (looksLikeDiff(part.output)) {
      const diff=diffPart({text:part.output});
      diff.className='diff tool-diff';
      body.append(diff);
    } else body.append(toolBlock('output',part.output));
  }
  if (part.approval) body.append(approvalRow(message,part));
  if (!body.children.length) {
    const note=document.createElement('p');
    note.className='tool-empty';
    note.textContent='No details yet.';
    body.append(note);
  }
  box.append(body);
  return box;
}
function errorPart(message,part) {
  const box=document.createElement('div');
  box.className='part error';
  const text=document.createElement('div');
  text.className='error-text';
  renderMarkdown(text,part.text||'The assistant failed');
  box.append(text);
  if (part.detail&&part.detail!==part.text) {
    const detail=document.createElement('pre');
    detail.className='error-detail';
    detail.textContent=truncateText(part.detail,800);
    box.append(detail);
  }
  if (part.hint) {
    const hint=document.createElement('div');
    hint.className='error-hint';
    renderMarkdown(hint,part.hint);
    box.append(hint);
  }
  if (part.retry!==false&&message.meta?.request) {
    const retry=document.createElement('button');
    retry.type='button'; retry.className='error-retry'; retry.textContent='Retry';
    retry.onclick=event=>{ event.preventDefault(); retryMessage(message); };
    box.append(retry);
  }
  return box;
}
function renderPart(message,part) {
  switch (part.type) {
    case 'reasoning': return reasoningPart(part);
    case 'tool': return toolPart(message,part);
    case 'diff': return diffPart(part);
    case 'error': return errorPart(message,part);
    default: return textPart(part);
  }
}

/* -- painting: one frame at a time, one window of rows -------------------- */

function heightOf(message) {
  if (!message) return ROW_ESTIMATE;
  const measured=rowHeights.get(message.id);
  if (typeof measured==='number'&&measured>0) return measured;
  const entry=mountedRows.get(message.id);
  const live=entry?.row.getBoundingClientRect().height;
  if (live&&live>0) return live;
  return ROW_ESTIMATE;
}
function sumHeights(from,to) {
  let total=0;
  for (let index=from;index<to;index+=1) total+=heightOf(messages[index]);
  return Math.round(total);
}
function indexAtOffset(offset) {
  let run=0;
  for (let index=0;index<messages.length;index+=1) {
    run+=heightOf(messages[index]);
    if (run>offset) return index;
  }
  return Math.max(0,messages.length-1);
}
function atBottom(log) {
  if (!log) return true;
  return log.scrollHeight-log.scrollTop-log.clientHeight<40;
}
/* Following the log is the reader's choice, changed only by the reader scrolling
 * (or sending). Geometry alone strands them: the inline toast shrinks the log
 * without a scroll, and the `scroll` event of Navi's own pin arrives a frame
 * later, measured against the shrunken log (42 px off the bottom), so it read
 * as the reader leaving. Only a position Navi did not set counts as the reader. */
let followLog=true, ownScrollTop=-1;
function pinLog(log) {
  log.scrollTop=log.scrollHeight;
  ownScrollTop=log.scrollTop;
  followLog=true;
}
function rememberHeight(id,node) {
  const height=node?.getBoundingClientRect().height;
  if (height&&height>0) rowHeights.set(id,height);
}
function rebuildLog(log,ids,top,bottom) {
  const above=document.createElement('div');
  above.className='chat-spacer'; above.style.height=top+'px';
  const below=document.createElement('div');
  below.className='chat-spacer'; below.style.height=bottom+'px';
  const children=[above];
  for (const id of ids) { const entry=mountedRows.get(id); if (entry) children.push(entry.row); }
  children.push(below);
  log.replaceChildren(...children);
  mountedOrder=ids.slice(); spacerTop=top; spacerBottom=bottom;
}
function updatePill(log) {
  const pill=element('chat-pill');
  if (!pill) return;
  pill.hidden=followLog||atBottom(log)||messages.length===0;
}

function paintChat() {
  chatFrame=0;
  const log=chatLog();
  if (!log) return;
  const total=messages.length;
  const pinned=followLog||atBottom(log);
  let start=0, end=total;
  if (total>RENDER_WINDOW*2+1) {
    if (pinned) {
      start=Math.max(0,total-(RENDER_WINDOW*2+1));
    } else {
      const anchor=indexAtOffset(log.scrollTop+log.clientHeight*0.4);
      start=Math.max(0,anchor-RENDER_WINDOW);
      end=Math.min(total,Math.max(start+RENDER_WINDOW*2+1,anchor+RENDER_WINDOW+1));
    }
  }
  const nextIds=[];
  for (let index=start;index<end;index+=1) {
    const message=messages[index];
    if (!message) continue;
    let entry=mountedRows.get(message.id);
    if (!entry) { entry={row:createRow(message),version:-1}; mountedRows.set(message.id,entry); }
    if (entry.version!==message.version) { renderRow(entry.row,message); entry.version=message.version; }
    nextIds.push(message.id);
  }
  for (const [id,entry] of [...mountedRows]) {
    if (nextIds.includes(id)) continue;
    rememberHeight(id,entry.row);
    mountedRows.delete(id);
  }
  const top=sumHeights(0,start), bottom=sumHeights(end,total);
  if (!sameList(nextIds,mountedOrder)||top!==spacerTop||bottom!==spacerBottom) rebuildLog(log,nextIds,top,bottom);
  if (pinned) pinLog(log);
  updatePill(log);
}
/* Paint now instead of on the next frame: paging has to measure the height it
 * added before the browser can show the jump. */
function flushChat() {
  if (chatFrame) { cancelAnimationFrame(chatFrame); chatFrame=0; }
  paintChat();
}

/* -- one visible surface per failure (BUG-036/039) ------------------------ */

function failureKey(text) { return String(text||'').trim().toLowerCase(); }
/* A failure that already owns a row (the failed turn) only marks the badge and the
 * de-duplication key; a failure with no row creates one system row. Either way the
 * user sees one surface per failure (BUG-036/039). */
function noteFailure(text) {
  lastFailure=String(text||'Request failed');
  needsAttention=true;
  failures.set(failureKey(lastFailure),Date.now());
  for (const [entry,at] of failures) if (Date.now()-at>60000) failures.delete(entry);
  updateStatus();
}
function surfaceFailure(text) {
  const message=String(text||'Request failed');
  lastFailure=message;
  needsAttention=true;
  updateStatus();
  const key=failureKey(message);
  const seen=failures.get(key);
  const now=Date.now();
  if (seen&&now-seen<2000) return null;
  failures.set(key,now);
  for (const [entry,at] of failures) if (now-at>60000) failures.delete(entry);
  const node=addMessage({
    role:'system', time:now, status:'failed',
    parts:[{type:'error',text:message,retry:false}],
    meta:{source:'host',project_id:activeProjectId()||undefined},
  });
  return node;
}
function warningRow(text) {
  const message=String(text||'Warning');
  const seen=failures.get('warn:'+failureKey(message));
  const now=Date.now();
  if (seen&&now-seen<2000) return null;
  failures.set('warn:'+failureKey(message),now);
  return addMessage({
    role:'system', time:now, status:'complete',
    parts:[{type:'text',text:message,tone:'warning'}],
    meta:{},
  });
}
function clearAttention() { if (needsAttention) { needsAttention=false; updateStatus(); } }

/* -- events -> messages --------------------------------------------------- */

/* A turn that `turn_state` marked done keeps its row so the following `reply` event
 * finalises it in place; `pendingFinalise` carries that row across the boundary when
 * a new turn has already started. */
let pendingFinalise=null;
function startTurn() {
  if (turn) {
    if (!turn.done) return turn;
    pendingFinalise=turn.message&&turn.message.status==='streaming'?turn.message:null;
  }
  turn={message:null,user:null,started:Date.now(),done:false};
  updateStatus();
  return turn;
}
/* Every command carries one request id from the RPC through each event it causes
 * (`working`, deltas, tools, `reply`, `error`) and the daemon's own answer. Its row
 * is keyed by that id: whichever copy arrives first draws it, later copies update
 * it, and a replay after a restart finds the stored row instead of adding one. */
function requestOf(event) {
  const id=event?.details?.request_id;
  return typeof id==='string'&&id?id:null;
}
const turnRowId=request=>'turn-'+request;
/* A request whose row is already final: its late `working`, or the second copy of
 * its reply, must not reopen the turn. */
function settledRow(request) {
  const row=request?messageById.get(turnRowId(request)):null;
  return row&&row.status!=='streaming'?row:null;
}
/* Without a request id, a reply may still carry a stable message id (`message_id`,
 * or `id` on a `reply`): one reply delivered twice is one row and two different
 * replies sharing text are two — the comparison is never made on text (BUG-046). */
function messageIdentity(event) {
  const details=event?.details||{};
  const request=requestOf(event);
  if (request) return turnRowId(request);
  if (typeof details.message_id==='string'&&details.message_id) return details.message_id;
  if (typeof details.id==='string'&&details.id&&event?.kind==='reply') return details.id;
  return null;
}
function assistantMessage(event) {
  startTurn();
  const identity=messageIdentity(event);
  if (turn.message&&(!identity||turn.message.id===identity)) return turn.message;
  const existing=identity?messageById.get(identity):null;
  /* The turn's own request is what Retry repeats: a background assessment is
   * not the user's last typed line. */
  const opened=event?.kind==='working'&&typeof event.details?.text==='string'?event.details.text.trim():'';
  turn.message=existing||addMessage({
    id:identity||undefined,
    role:'assistant', time:Number(event?.time)||Date.now(), status:'streaming',
    parts:[],
    meta:{request:opened||activeText||undefined,project_id:activeProjectId()||undefined},
  });
  return turn.message;
}
function finalMessage(event) {
  const request=requestOf(event);
  const own=request?messageById.get(turnRowId(request)):null;
  if (own) {
    if (pendingFinalise===own) pendingFinalise=null;
    return own;
  }
  if (pendingFinalise&&pendingFinalise.status==='streaming') {
    const message=pendingFinalise;
    pendingFinalise=null;
    return message;
  }
  if (turn?.message) return turn.message;   // the row turn_state already filled
  const identity=messageIdentity(event);
  const existing=identity?messageById.get(identity):null;
  if (existing) return existing;
  return assistantMessage(event);
}
function setTurnLabel(label) {
  const next=label||'';
  if (turnLabel===next) return;
  turnLabel=next;
  updateStatus();
}
function closeTurn() {
  turn=null;
  turnLabel='';
  setBusy(false);
  setPhase('idle');
  updateStatus();
}

function beginTurn(event,reasoning) {
  const requestId=requestOf(event);
  if (settledRow(requestId)) return;
  /* A spoken command settles the daemon's pending question too. */
  if (!event.replayed&&choiceOffer&&requestId!==choiceOffer.request) clearChoices();
  setBusy(true);
  setPhase('working');
  startTurn();
  if (requestId) turn.request=requestId;
  if (request&&request.queued) promoteQueued();
  setTurnLabel(reasoning?'Thinking':'Working');
  assistantMessage(event);
}
function promoteQueued() {
  const user=request?.user;
  request.queued=false;
  if (!user) return;
  user.status='complete';
  bump(user);
}
function reasoningDelta(event) {
  if (settledRow(requestOf(event))) return;
  const details=event.details||{};
  const text=typeof details.text==='string'?details.text:(event.message||'');
  if (!text) return;
  const message=assistantMessage(event);
  let part=message.parts.at(-1);
  if (!part||part.type!=='reasoning') { part={type:'reasoning',text:''}; message.parts.push(part); }
  part.text+=text;
  setTurnLabel('Thinking');
  bump(message);
}
function replyDelta(event) {
  /* A delta polled after the RPC answer already finished its row is stale. */
  if (settledRow(requestOf(event))) return;
  const details=event.details||{};
  const text=typeof details.text==='string'?details.text:(event.message||'');
  const message=assistantMessage(event);
  const part=lastTextPart(message,true);
  /* A per-chunk delta appends; a cumulative payload (``turn``/``done``) replaces.
   * Both shapes are accepted so the producer can coalesce without a UI change. */
  const replace=details.cumulative===true||details.done===true||
    details.replace===true||typeof details.turn==='string';
  part.text=replace?text:(part.text||'')+text;
  if (replace&&typeof details.turn==='string') message.turn=details.turn;   // bind the turn
  message.status='streaming';
  setTurnLabel('Replying');
  setBusy(true);
  bump(message);
}
/* Close what belongs to `requestId`: the pending dispatch and the open turn, but
 * never a newer command's that started after this one's answer. */
function finishRequest(requestId) {
  if (request&&(!requestId||!request.id||request.id===requestId)) request.done=true;
  if (turn&&(!requestId||!turn.request||turn.request===requestId)) closeTurn();
}
function replyDone(event) {
  const details=event.details||{};
  const requestId=requestOf(event);
  const text=typeof details.text==='string'?details.text:(event.message||'');
  const settled=settledRow(requestId);
  if (settled&&settled.status==='complete'&&messageText(settled)===text.trim()) {
    finishRequest(requestId);
    return;
  }
  /* A reply that switched project is stored in the project it was asked in.
   * Once the station has moved, a second delivery (the feed after the RPC
   * result) finds no row to match in the new transcript and must not add one. */
  if (typeof details.switch_project==='string'&&details.switch_project===activeProjectId()
    &&details.project_id&&details.project_id!==activeProjectId()) {
    if (requestId) dropEmptyRow(turnRowId(requestId));
    finishRequest(requestId);
    return;
  }
  const message=finalMessage(event);
  const part=lastTextPart(message,true);
  part.text=text;                     // the finished answer owns the row
  const source=typeof details.source==='string'?details.source:'';
  const stopped=details.stopped===true||source==='stopped';
  message.status=stopped?'stopped':'complete';
  message.meta={
    ...(message.meta||{}),
    request:message.meta?.request||activeText||undefined,
    model:normalizeModel(details.actual_model),
    requested:modelRef()||undefined,
    agent:typeof details.actual_agent==='string'?details.actual_agent:undefined,
    usage:details.usage||undefined,
    sessionUsage:details.session_usage||undefined,
    finish:typeof details.finish==='string'?details.finish:undefined,
    session:typeof details.session==='string'?details.session:undefined,
    source:source||undefined,
    durationMs:turn&&turn.started?Date.now()-turn.started:undefined,
    project_id:details.project_id||activeProjectId()||undefined,
    warnings:Array.isArray(details.warnings)&&details.warnings.length?details.warnings:undefined,
  };
  if (Array.isArray(details.warnings)) for (const warning of details.warnings) warningRow(String(warning));
  if (!event.replayed&&Array.isArray(details.choices)&&details.choices.length) offerChoices(message,details.choices,requestId);
  if (Array.isArray(details.attachments)&&details.attachments.length) {
    warningRow('Attachments: '+details.attachments.length+' file'+(details.attachments.length===1?'':'s')+' used for this turn');
  }
  finishRequest(requestId);
  const saved=saveMessage(message);
  clearAttention();
  bump(message);
  /* A spoken "show the progress" opens the panel; any answer about progress
   * may have moved it, so the HUD re-reads. */
  if (!event.replayed) {
    /* "Switch to acme-api": the active project is window state, so the reply
     * carries the switch. The row is stored first, because switching re-reads
     * the transcript of the project it moves to. */
    const target=typeof details.switch_project==='string'?details.switch_project:'';
    if (target&&target!==activeProjectId()) {
      Promise.resolve(saved).finally(()=>{
        switchProject(target);
        /* The new project's transcript replaces the view; the answer stays
         * readable for a moment. */
        toast(text);
        if (details.action==='show_graph') showGraph(true);
      });
      return;
    }
    if (details.action==='show_graph') showGraph(true);
    if (details.action==='hide_graph') showGraph(false);
    const fresh=Number(event.time||Date.now())>=PAGE_START;
    if (fresh&&details.action==='show_settings') openSettings();
    if (fresh&&details.action==='add_project') openProjectDialog();
    const screenModes={fullscreen:'toggle',fullscreen_on:'on',fullscreen_off:'off'};
    if (fresh&&screenModes[details.action]) send({type:'fullscreen',mode:screenModes[details.action]});
    if (details.action==='show_graph'||details.progress_recorded>0) scheduleGraph(300);
    const project=String(details.project_id||'');
    if (assessing&&assessing.via==='assistant'&&project===assessing.project&&details.source!=='navi') endAssessment(project);
  }
}
/* The daemon reports the answering model; a fallback is visible when it differs
 * from the stored request (BUG-010). */
function normalizeModel(actual) {
  if (!actual) return undefined;
  if (typeof actual==='string') return actual;
  if (typeof actual!=='object') return undefined;
  const provider=actual.providerID||actual.provider;
  const id=actual.id||actual.modelID;
  if (!id) return undefined;
  return (provider?provider+'/':'')+id;
}
function toolUse(event) {
  if (settledRow(requestOf(event))) return;
  const details=event.details||{};
  const message=assistantMessage(event);
  const id=details.id===undefined||details.id===null?null:String(details.id);
  let part=id?message.parts.find(entry=>entry.type==='tool'&&entry.id===id&&!entry.approval):null;
  if (!part) {
    part={type:'tool',id:id||uid('tool'),name:details.name||details.tool||'tool',status:details.status||'running'};
    message.parts.push(part);
  }
  if (details.name||details.tool) part.name=details.name||details.tool;
  if (details.status) part.status=details.status;
  if (details.input!==undefined) part.input=details.input;
  if (details.output!==undefined) part.output=details.output;
  if (details.exit!==undefined) part.exit=details.exit;
  if (details.progress!==undefined) part.progress=details.progress;
  /* One final per tool id: the same record is upserted, never appended twice. */
  if (details.final===true) part.final=true;
  setBusy(true);
  setTurnLabel('Working · '+part.name);
  bump(message);
}
function approvalEvent(event) {
  if (settledRow(requestOf(event))) return;
  const details=event.details||{};
  const message=assistantMessage(event);
  const permissionId=details.id===undefined||details.id===null?uid('approval'):String(details.id);
  let part=message.parts.find(entry=>entry.type==='tool'&&entry.approval&&entry.approval.id===permissionId);
  if (!part) {
    part={
      type:'tool', id:'approval-'+permissionId, name:details.tool||details.name||'Tool',
      status:'awaiting approval', input:details.input,
      approval:{id:permissionId,tool:details.tool||details.name||'',decision:null},
    };
    message.parts.push(part);
  }
  setBusy(true);
  setTurnLabel('Approval needed');
  bump(message);
}
function failedEvent(event) {
  const details=event.details||{};
  const text=String(event.message||details.error||details.text||'Assistant unavailable');
  const requestId=requestOf(event);
  const op=typeof details.op==='string'?details.op:'';
  if (assessing&&String(event.project_id||'')===assessing.project&&/assessment/i.test(text)) {
    endAssessment(assessing.project);
  }
  /* Another caller's failed call (a harness's progress report, a panel edit that
   * already showed its own error) is not this conversation's failure. */
  if (!requestId&&op&&op!=='command') { record('warn',op+' failed: '+text); return; }
  if (requestId) { requestFailed(event,requestId,text); return; }
  /* An error with neither (a worker that keeps crashing) is a notice, not the
   * answer to whatever turn happens to be open. */
  if (event.replayed) return;
  surfaceFailure(text);
  setPhase('error');
}
/* The failure of one request lands on that request's row, drawn now when no
 * event of it arrived yet; a replayed failure only updates a row on screen. */
function requestFailed(event,requestId,text) {
  const id=turnRowId(requestId);
  let row=messageById.get(id)||null;
  if (row&&row.status==='failed') { finishRequest(requestId); return; }
  if (!row&&turn?.message&&turn.request===requestId) row=turn.message;
  if (!row) {
    if (event.replayed) return;
    row=addMessage({
      id, role:'assistant', time:Number(event.time)||Date.now(), status:'failed', parts:[],
      meta:{request:activeText||undefined,project_id:event.project_id||activeProjectId()||undefined},
    });
  }
  markFailed(row,text);
  saveMessage(row);
  finishRequest(requestId);
  if (!event.replayed) alarm(text);
  bump(row);
}
/* Navi's assistant runs on OpenCode; without it the deterministic commands
 * still work, so the row says which ones instead of only reporting the error. */
const ASSISTANT_DOWN=/OpenCode (is not installed|is not running|did not start|is not reachable)/;
const ASSISTANT_HINT='Navi’s own commands still work: “open Claude Code”, “how far along is this project?”, '
  +'“mark Voice done”, /assess. Chatting with Navi needs OpenCode: install the `opencode` CLI or start `opencode serve`.';
/* A usage or lookup error is the command's answer: retrying it changes nothing.
 * Only a transport failure means the assistant itself is unavailable. */
function failurePart(text) {
  const usage=/^(Use \/|Use at most|No (task|part|Navi project|project)|Unknown |Missing |This project|Progress needs|Command must)/.test(text);
  return usage
    ?{part:{type:'error',text:'Could not do that',detail:text,retry:false},source:'navi'}
    :{part:{type:'error',text:'Assistant unavailable',detail:text,retry:true,
      hint:ASSISTANT_DOWN.test(text)?ASSISTANT_HINT:undefined},source:'unavailable'};
}
/* A failure the user must act on (the assistant is down) raises attention; a
 * command that could not be done is only its answer. */
function alarm(text) {
  if (failurePart(text).source!=='unavailable') return;
  setPhase('error');
  noteFailure(text);
}
function markFailed(row,text) {
  const failure=failurePart(text);
  row.status='failed';
  row.parts=row.parts.filter(part=>part.type!=='error'&&!(part.type==='text'&&!String(part.text||'').trim()));
  row.parts.push(failure.part);
  row.meta={...(row.meta||{}),request:row.meta?.request||activeText||undefined,source:failure.source};
}
function stoppedEvent(event) {
  const details=event.details||{};
  const own=requestOf(event)?messageById.get(turnRowId(requestOf(event))):null;
  const open=own||turn?.message;
  if (open) {
    if (open.status==='streaming') open.status='stopped';
    bump(open);
  }
  if (Number(details.queued_dropped)>0) warningRow(Number(details.queued_dropped)+' queued request(s) dropped');
  setBusy(false);
  setPhase('idle');
  updateStatus();
}
function queuedEvent(event) {
  const details=event.details||{};
  const position=Number(details.position)||0;
  warningRow('Queued behind the running turn'+(position?' · position '+position:''));
}
function launchedEvent(event) {
  const details=event.details||{};
  toast('Harness session started: '+(details.name||details.id||'session'));
  addMessage({
    role:'system', time:Number(event.time)||Date.now(), status:'complete',
    parts:[{type:'text',text:'Harness session started: '+(details.name||details.id||'session'),tone:'tool'}],
    meta:{},
  });
}
/* A prompt Navi held for a busy session reached it. */
function deliveredEvent(event) {
  const text=String(event.message||'Prompt sent');
  toast(text);
  addMessage({
    role:'system', time:Number(event.time)||Date.now(), status:'complete',
    parts:[{type:'text',text,tone:'tool'}],
    meta:{},
  });
}
function transcriptEvent(event) {
  /* A transcript event opens the user turn when the request came from outside the
     composer (voice, host). Its id is the request id, so a re-sent copy merges. */
  const details=event.details||{};
  const text=String(details.text||event.message||'').trim();
  if (!text) return;
  const id=details.request_id||details.id||uid('u');
  if (messageById.has(id)||event.replayed) return;
  const row=addMessage({
    id, role:'user', time:Number(event.time)||Date.now(), status:'complete',
    parts:[{type:'text',text}],
    meta:{source:details.source||'transcript',project_id:event.project_id||activeProjectId()||undefined},
  });
  saveMessage(row);
  activeText=text;
}
function warningEvent(event) {
  const message=String(event.message||event.details?.text||'Warning');
  if (!event.replayed) warningRow(message);
  const project=String(event.project_id||event.details?.project_id||'');
  if (assessing&&project===assessing.project&&/assessment|PROGRESS lines/i.test(message)) endAssessment(project);
}
/* A progress report is one compact row: who moved what, and on what evidence.
 * The user's own changes get no row (the panel or the spoken reply already
 * shows them), and reports for another project only refresh its switcher. */
function progressEvent(event) {
  const details=event.details||{};
  const project=String(details.project_id||event.project_id||'');
  const active=activeProjectId();
  if (!event.replayed&&details.source!=='user'&&(!project||project===active)) {
    const message=capitalise(String(event.message||'Progress updated'));
    const evidence=details.scan===true||details.assessment?'':String(details.evidence||'').trim();
    const line=evidence?`${message} — ${evidence}`:message;
    const parts=[{type:'text',text:line.length>420?line.slice(0,419)+'…':line,tone:'progress'}];
    if (details.note) parts.push({type:'text',text:String(details.note),tone:'warning'});
    addMessage({role:'system',time:Number(event.time)||Date.now(),status:'complete',parts,
      meta:{project_id:project||undefined,source:String(details.source||'')||undefined}});
  }
  if (details.assessment&&project&&!event.replayed) beginAssessment(project,details.assessment);
  if (details.scan===true&&details.digest&&project) {
    scans.set(project,{digest:String(details.digest),time:Number(event.time)||Date.now()});
  }
  /* The whole-project line is the last one an assessment reports. */
  const overall=details.value!==null&&details.value!==undefined&&!details.aspect_id&&!details.task_id;
  if (overall&&details.source!=='detector') endAssessment(project);
  if (project===active) scheduleGraph(event.replayed?1200:250);
  scheduleOverview(event.replayed?2500:1200);
}
/* Each step of a command reports `command`; the last one ends the turn of that
 * request (its reply, when it has one, follows and fills the row). */
function commandEvent(event) {
  const details=event.details||{};
  const steps=Number(details.steps), step=Number(details.step);
  if (Number.isFinite(steps)&&Number.isFinite(step)&&step+1<steps) return;
  const requestId=requestOf(event);
  if (turn&&(!requestId||!turn.request||turn.request===requestId)) closeTurn();
}
function handleEvent(event) {
  if (!event||typeof event!=='object') return;
  switch (event.kind) {
    case 'working': beginTurn(event,false); break;
    case 'thinking': beginTurn(event,true); break;
    case 'reasoning': reasoningDelta(event); break;
    case 'reply_delta': replyDelta(event); break;
    case 'reply': replyDone(event); break;
    case 'tool_use': toolUse(event); break;
    case 'approval': approvalEvent(event); break;
    /* The feed replays from the start on every boot; a row that only narrates
     * (a warning, a queue note, a launch, a progress report) is drawn for live
     * events only, or each restart would re-append the whole history. */
    case 'warning': warningEvent(event); break;
    case 'queued': if (!event.replayed) queuedEvent(event); break;
    case 'error': failedEvent(event); break;
    case 'stopped': stoppedEvent(event); break;
    case 'launched': if (!event.replayed) launchedEvent(event); break;
    case 'delivered': if (!event.replayed) deliveredEvent(event); break;
    case 'progress': progressEvent(event); break;
    case 'transcript': transcriptEvent(event); break;
    /* A harness tail that produced evidence moves the graph; the panel and the
     * HUD follow without a reload (BUG-087/089). */
    case 'harness': scheduleGraph(800); break;
    case 'command': commandEvent(event); break;
    case 'ready': break;   // the boot line carries no user-visible state
    default: break;        // a newer kind renders nothing instead of throwing
  }
}

/* -- persistence and paging ---------------------------------------------- */

/* -- project scope: one active project drives cwd, graph, assistant, status -- */

function projectList() {
  return Array.isArray(snapshot?.projects)?snapshot.projects:[];
}
function activeProjectId() {
  const list=projectList();
  if (!list.length) return null;
  if (activeProject&&list.some(project=>project.id===activeProject)) return activeProject;
  const remembered=readProject();
  if (remembered&&list.some(project=>project.id===remembered)) {
    activeProject=remembered;
    return remembered;
  }
  const first=list[0]?.id||null;
  if (first) { activeProject=first; writeProject(first); }
  return first;
}
function activeProjectRow() {
  const id=activeProjectId();
  return id?projectList().find(project=>project.id===id)||null:null;
}
function pathState(project) { return String(project?.path_state||'ok'); }
/* A folder that is where it should be needs no badge; only a problem shows. */
function pathNote(project) {
  const state=pathState(project);
  return state==='ok'?'':state==='missing'?'folder missing':state;
}

/* The switcher lives in the strip: a button naming the project and its path
 * state, and a menu of every registered project. */
let switcherShell=null, switcherOpen=false, switcherSignature='';
let assistantScope=null;

function projectSwitcher() {
  const bar=element('project-switcher');
  if (!bar) return null;
  if (switcherShell) return switcherShell;
  const button=document.createElement('button');
  button.type='button';
  button.className='project-button';
  button.setAttribute('aria-haspopup','listbox');
  button.setAttribute('aria-expanded','false');
  const name=document.createElement('span'); name.className='project-name';
  const percent=document.createElement('span'); percent.className='project-percent';
  const state=document.createElement('span'); state.className='project-path-state';
  const caret=document.createElement('span'); caret.className='project-caret'; caret.textContent='▾';
  button.append(name,percent,state,caret);
  button.addEventListener('click',event=>{
    event.stopPropagation();
    if (!projectList().length) { openProjectDialog(); return; }
    switcherOpen=!switcherOpen;
    renderProjectSwitcher();
  });
  const menu=document.createElement('div');
  menu.className='project-menu';
  menu.hidden=true;
  menu.setAttribute('role','listbox');
  menu.setAttribute('aria-label','Projects');
  bar.append(button,menu);
  document.addEventListener('click',event=>{
    if (!switcherOpen) return;
    if (bar.contains(event.target)) return;
    switcherOpen=false;
    renderProjectSwitcher();
  });
  bar.addEventListener('keydown',event=>{
    if (event.key!=='Escape'||!switcherOpen) return;
    switcherOpen=false;
    renderProjectSwitcher();
    button.focus();
  });
  switcherShell={bar,button,menu,name,percent,state};
  return switcherShell;
}
/* A project's headline from the overview, or '' while it has none. */
function overviewLabel(id) {
  const row=overview.get(String(id||''));
  const value=row?Number(row.percent):NaN;
  return row&&row.percent!==null&&row.percent!==undefined&&Number.isFinite(value)?wholePercent(value):'';
}

function renderProjectSwitcher(force) {
  const shell=projectSwitcher();
  if (!shell) return;
  const list=projectList();
  const active=activeProjectRow();
  const signature=list.map(project=>`${project.id}:${project.path_state||'ok'}:${overviewLabel(project.id)}:${overview.get(String(project.id))?.commits_since||0}`).join('|')
    +`#${active?.id||''}#${switcherOpen?'1':'0'}#${list.length}`;
  if (!force&&signature===switcherSignature) return;
  switcherSignature=signature;
  /* With no project the switcher is how one gets added, so it never hides. */
  shell.bar.hidden=false;
  shell.name.textContent=active?active.name:'Add a project';
  shell.percent.textContent=active?overviewLabel(active.id):'';
  shell.percent.hidden=!shell.percent.textContent;
  const activeSince=active?commitsSince(overview.get(String(active.id))):'';
  shell.percent.classList.toggle('stale',!!activeSince&&!!shell.percent.textContent);
  shell.percent.title=activeSince?`${activeSince} the estimate`:'';
  shell.state.textContent=active?pathNote(active):'';
  shell.state.dataset.state=active?pathState(active):'none';
  shell.button.title=active
    ?`${active.path}\nCtrl+1..9 switches project`
    :'Add a project folder for Navi to track';
  shell.button.setAttribute('aria-expanded',String(switcherOpen));
  shell.menu.hidden=!switcherOpen;
  if (!switcherOpen) { shell.menu.replaceChildren(); return; }
  const add=document.createElement('button');
  add.type='button';
  add.className='project-add';
  add.textContent='Add project…';
  add.addEventListener('click',event=>{
    event.stopPropagation();
    switcherOpen=false;
    renderProjectSwitcher(true);
    openProjectDialog();
  });
  shell.menu.replaceChildren(...list.map((project,index)=>{
    const option=document.createElement('button');
    option.type='button';
    option.className='project-option';
    option.dataset.project=String(project.id||'');
    option.setAttribute('role','option');
    option.setAttribute('aria-selected',String(project.id===active?.id));
    const name=document.createElement('span'); name.className='project-name'; name.textContent=project.name||'project';
    const path=document.createElement('span'); path.className='project-path'; path.textContent=project.path||'';
    const badge=document.createElement('span'); badge.className='project-path-state';
    badge.dataset.state=pathState(project); badge.textContent=pathNote(project);
    const key=document.createElement('span'); key.className='project-key';
    key.textContent=index<9?'Ctrl+'+(index+1):'';
    const percent=document.createElement('span'); percent.className='project-percent';
    percent.textContent=overviewLabel(project.id)||'—';
    const since=commitsSince(overview.get(String(project.id||'')));
    percent.classList.toggle('stale',!!since&&percent.textContent!=='—');
    percent.title=since?`Completion · ${since} the estimate`:'Completion';
    option.append(name,path,percent,badge,key);
    option.addEventListener('click',event=>{
      event.stopPropagation();
      switcherOpen=false;
      renderProjectSwitcher(true);
      switchProject(String(project.id||''));
    });
    return option;
  }),add);
}

/* Add project: a folder (typed, or from the host's native chooser), a name that
 * follows the folder until the user edits it, and an optional first assessment. */
const projectDialog=element('project-dialog');
let projectNameEdited=false;
function folderName(path) {
  return String(path||'').replace(/[\\/]+$/,'').split(/[\\/]/).pop()||'';
}
function projectNote(text,error) {
  const note=element('project-note');
  note.textContent=text||'';
  note.classList.toggle('error',!!error);
}
function openProjectDialog(path) {
  element('project-path').value=path||'';
  element('project-name').value=folderName(path);
  projectNameEdited=false;
  element('project-assess').checked=true;
  projectNote('Navi scans the files, then asks a linked harness (or its assistant) to estimate each part.');
  projectDialog.showModal();
  element('project-path').focus();
}
/* A native folder chooser from the host window; resolves to a path or null. */
function pickFolder(start) {
  const ticket=++requests;
  return new Promise((resolve,reject)=>{
    const budget=10*60*1000;   // the person may browse for a while
    pending.set(ticket,{op:'pick-folder',resolve,reject,budget,expires:Date.now()+budget});
    if (!send({type:'pick-folder',ticket,start:String(start||'')})) {
      pending.delete(ticket);
      reject(new Error('The folder chooser needs the Navi window'));
    }
  });
}
element('project-path').addEventListener('input',()=>{
  if (!projectNameEdited) element('project-name').value=folderName(element('project-path').value.trim());
});
element('project-name').addEventListener('input',()=>{ projectNameEdited=!!element('project-name').value.trim(); });
element('project-browse').onclick=async()=>{
  try {
    const result=await pickFolder(element('project-path').value.trim()||activeProjectRow()?.path||'');
    if (!result?.path) return;
    element('project-path').value=result.path;
    if (!projectNameEdited) element('project-name').value=folderName(result.path);
    projectNote('');
  } catch (error) { projectNote(error.message,true); }
};
element('project-cancel').onclick=()=>projectDialog.close();
element('project-form').onsubmit=async event=>{
  event.preventDefault();
  const path=element('project-path').value.trim();
  const name=element('project-name').value.trim()||folderName(path);
  if (!path) { projectNote('Choose the project folder',true); element('project-path').focus(); return; }
  try {
    const project=(await rpc('project_add',{name,path}))?.project;
    if (!project?.id) throw new Error('The daemon did not return the project');
    /* The poll that carries the new list may already be in flight, so the row is
     * added here and the next poll replaces it with the daemon's copy. */
    if (snapshot&&!projectList().some(row=>row.id===project.id)) snapshot.projects=[...projectList(),project];
    projectDialog.close();
    switchProject(project.id);
    syncProgressScope();
    toast('Added '+(project.name||name));
    if (element('project-assess').checked) assessProject();
  } catch (error) { projectNote(error.message,true); }
};

function updateProjectLine() {
  const node=element('project-status');
  if (!node) return;
  const project=activeProjectRow();
  node.textContent=project?project.name:'No project';
  node.dataset.state=project?pathState(project):'none';
  node.title=project
    ?[project.path,[pathNote(project),project.git_head?String(project.git_head).slice(0,12):''].filter(Boolean).join(' · ')].filter(Boolean).join('\n')
    :'Add a project to scope the work';
}

/* One click changes what "this project" means everywhere: the new-session
 * default cwd, the graph, the assistant session, and the status line (BUG-090). */
function switchProject(id) {
  const project=projectList().find(entry=>entry.id===id);
  if (!project) return null;
  const changed=activeProject!==id;
  activeProject=id;
  writeProject(id);
  renderProjectSwitcher(true);
  updateProjectLine();
  renderSessions(snapshot?.sessions||[]);
  loadGraph();
  if (!changed) return project;
  /* The assistant session is project-scoped: the daemon rebinds the directory
   * its next turn, permission reply and interrupt use, without discarding the
   * session the project already owns. */
  rpc('assistant_scope',{project_id:id})
    .then(scope=>{ assistantScope=scope; updateProjectLine(); })
    .catch(error=>record('warn','assistant scope failed: '+error.message));
  /* The transcript belongs to the project, so it is re-hydrated from its rows
   * instead of being carried across. */
  resetChat();
  loadHistory();
  return project;
}

/* Ctrl+1..9 selects the project at that position. */
addEventListener('keydown',event=>{
  if (!event.ctrlKey||event.metaKey||event.altKey||event.shiftKey) return;
  if (!/^[1-9]$/.test(event.key)) return;
  const target=projectList()[Number(event.key)-1];
  if (!target) return;
  event.preventDefault();
  switchProject(String(target.id||''));
});

/* "/" from outside any field starts a command, as in most chat tools. */
addEventListener('keydown',event=>{
  if (event.key!=='/'||event.ctrlKey||event.metaKey||event.altKey||event.defaultPrevented) return;
  if (event.target?.closest?.('input,textarea,select,[contenteditable],#terminal-pane,dialog')) return;
  const input=commandInput();
  if (!input||input.disabled) return;
  event.preventDefault();
  input.focus();
  if (!input.value) {
    input.value='/';
    input.dispatchEvent(new Event('input',{bubbles:true}));
  }
});

/* Escape closes the progress panel when nothing nearer used the key: an open
 * dialog, the slash menu, a confirm window and the terminal keep their own. */
addEventListener('keydown',event=>{
  if (event.key!=='Escape'||event.defaultPrevented||!graphOpen) return;
  if (document.querySelector('dialog[open]')||event.target?.closest?.('#terminal-pane')) return;
  event.preventDefault();
  showGraph(false);
  element('hud-open')?.focus();
});

function serialiseParts(parts) {
  return (parts||[]).map(part=>{
    if (part.type==='tool') {
      return {
        type:'tool',id:part.id,name:part.name,status:part.status,exit:part.exit,
        final:!!part.final,
        input:part.input===undefined?undefined:truncateText(fieldText(part.input),TOOL_TEXT_LIMIT),
        output:part.output===undefined?undefined:truncateText(fieldText(part.output),STORE_TEXT_LIMIT),
        approval:part.approval||undefined,
      };
    }
    if (part.type==='text') return {type:'text',text:String(part.text||''),tone:part.tone};
    if (part.type==='reasoning') return {type:'reasoning',text:String(part.text||'')};
    if (part.type==='error') return {type:'error',text:String(part.text||''),detail:part.detail?truncateText(String(part.detail),2000):undefined,retry:!!part.retry,hint:part.hint||undefined};
    if (part.type==='diff') return {type:'diff',path:part.path,text:String(part.text||'')};
    return {type:'text',text:String(part.text||'')};
  });
}
function saveMessage(message) {
  if (!message||!message.id) return;
  if (message.role!=='user'&&message.role!=='assistant') return;
  const payload={
    id:message.id,
    role:message.role,
    parts:serialiseParts(message.parts),
    time:message.time,
    turn:message.turn===null||message.turn===undefined?undefined:String(message.turn),
    project_id:message.meta?.project_id||activeProjectId()||undefined,
    session_id:message.meta?.session||undefined,
  };
  return rpc('message_save',payload).catch(error=>record('warn','message save failed: '+error.message));
}
function oldestLoadedCursor() {
  let cursor=null;
  for (const message of messages) {
    if (!Number.isFinite(message.cursor)) continue;
    if (cursor===null||message.cursor<cursor) cursor=message.cursor;
  }
  return cursor;
}
function historyParams(limit,before) {
  const params={limit};
  const project=activeProjectId();
  if (project) params.project_id=project;
  if (before!==null&&before!==undefined) params.before=before;
  return params;
}
/* Hydration reads the newest page and remembers the oldest cursor it holds;
 * `before` asks for the page in front of it, so a restart cannot skip a row and
 * a repeated reply cannot duplicate one (BUG-047). */
async function loadHistory(options={}) {
  const before=options.before===undefined?null:options.before;
  const limit=Math.max(1,Number(options.limit)||historyLimit);
  try {
    const result=await rpc('messages',historyParams(limit,before));
    const rows=Array.isArray(result&&result.messages)?result.messages:[];
    hydrateMessages(rows,before===null?{}:{older:true});
    oldestCursor=oldestLoadedCursor();
    historyHasMore=!!result&&result.has_more===true;
    if (before===null) historyLimit=Math.min(HISTORY_MAX,limit);
    updateEmptyState();
    return rows;
  } catch (error) {
    record('warn','chat history unavailable: '+error.message);
    return [];
  }
}
/* Scrolling to the top pages backwards from the oldest cursor. The prepended page
 * only keeps the reader's place if the scroll offset grows by the height it
 * added, so the paint is forced and the offset corrected in the same frame. */
async function loadOlder() {
  if (pagingHistory||!historyHasMore||oldestCursor===null) return;
  pagingHistory=true;
  const log=chatLog();
  const height=log?log.scrollHeight:0, top=log?log.scrollTop:0;
  try {
    const rows=await loadHistory({limit:historyLimit,before:oldestCursor});
    if (log&&rows.length) {
      flushChat();
      log.scrollTop=top+(log.scrollHeight-height);
      ownScrollTop=log.scrollTop;
    }
  } finally { pagingHistory=false; }
}

/* Isolated-only: drop the local transcript so a test can prove that hydration
 * rebuilds it from the daemon's store, not from a DOM snapshot. */
function resetChat() {
  messages.length=0;
  messageById.clear();
  rowHeights.clear();
  mountedRows.clear();
  mountedOrder=[];
  spacerTop=0;
  spacerBottom=0;
  const log=chatLog();
  if (log) log.replaceChildren();
  turn=null;
  request=null;
  pendingFinalise=null;
  activeText='';
  turnLabel='';
  busy=false;
  failures.clear();
  historyLimit=HISTORY_START;
  oldestCursor=null;
  historyHasMore=false;
  pagingHistory=false;
  updateEmptyState();
  updateStatus();
}

/* -- status: a projection of the payload, not a side effect of an event --- */

const STATUS_WORDS={ready:'Ready',working:'Working',attention:'Attention',offline:'Host not responding'};
let statusPhase='ready', lastFailure='', polledOnce=false;
function deriveStatus() {
  if (offline) return 'offline';
  if (busy||(turn&&!turn.done)) return 'working';
  if (needsAttention) return 'attention';
  return 'ready';
}
function updateStatus() {
  const next=deriveStatus();
  statusPhase=next;
  const node=element('state');
  if (node) {
    /* Never "starting": the label is either derived from the last poll or the
     * page has not polled yet, which reads as connecting (BUG-034). */
    node.dataset.phase=next;
    /* Before the first payload the only honest label is "connecting"; the four
     * derived phases take over from the first poll onward (BUG-034). */
    node.textContent=!polledOnce&&next==='ready'
      ?'Connecting…'
      :(next==='working'?(turnLabel||STATUS_WORDS.working):STATUS_WORDS[next]);
    node.title=next==='offline'?'The host is not answering status requests'
      :(lastFailure||'The status line follows the live daemon payload');
  }
  const stop=element('stop');
  if (stop) stop.hidden=!(busy||turn);
}

/* -- dispatch: one turn, one user row, one outcome ----------------------- */

function base64Bytes(data) {
  const clean=String(data||'').replace(/\s+/g,'');
  if (!clean) return 0;
  const pad=clean.endsWith('==')?2:clean.endsWith('=')?1:0;
  return Math.floor(clean.length*3/4)-pad;
}
function attachmentNameOk(name) {
  return !!name&&name.length<=ATTACH_NAME_MAX&&!/[\u0000-\u001f\u007f]/.test(name);
}
function renderAttachments() {
  const bar=element('attachment-chips');
  if (!bar) return;
  bar.hidden=!attachments.length;
  bar.replaceChildren(...attachments.map((item,index)=>{
    const chip=document.createElement('span');
    chip.className='attachment-chip'; chip.setAttribute('role','listitem');
    if (item.error) chip.dataset.state='error';
    const name=document.createElement('span'); name.className='attachment-name'; name.textContent=item.name;
    const size=document.createElement('span'); size.className='attachment-size';
    size.textContent=item.error?'unreadable':item.data?Math.max(1,Math.round(item.size/1024))+' KiB':'reading…';
    const remove=document.createElement('button');
    remove.type='button'; remove.className='attachment-remove'; remove.textContent='×';
    remove.setAttribute('aria-label','Remove '+item.name);
    remove.addEventListener('click',()=>{ attachments.splice(index,1); renderAttachments(); });
    chip.append(name,size,remove);
    return chip;
  }));
}
function acceptAttachment(item) {
  if (attachments.length>=ATTACH_MAX) {
    surfaceFailure('At most '+ATTACH_MAX+' attachments per prompt');
    return false;
  }
  const name=String(item&&item.name||'').trim();
  if (!attachmentNameOk(name)) { surfaceFailure('That attachment has an unusable name'); return false; }
  const data=String(item&&item.data||'').replace(/\s+/g,'');
  if (!data||!/^[A-Za-z0-9+/]*={0,2}$/.test(data)) { surfaceFailure(name+' is not valid base64'); return false; }
  const size=base64Bytes(data);
  if (size>ATTACH_BYTES) {
    surfaceFailure(name+' is larger than 2 MiB');
    return false;
  }
  attachments.push({name,size,data,error:null,pending:null});
  renderAttachments();
  return true;
}
function attachFiles(files) {
  for (const file of [...files||[]]) {
    if (attachments.length>=ATTACH_MAX) {
      surfaceFailure('At most '+ATTACH_MAX+' attachments per prompt');
      break;
    }
    const name=String(file&&file.name||'file');
    if (!attachmentNameOk(name)) { surfaceFailure('That attachment has an unusable name'); continue; }
    if (Number(file.size)>ATTACH_BYTES) {
      surfaceFailure(name+' is larger than 2 MiB');
      continue;
    }
    const entry={name,size:Number(file.size)||0,data:null,error:null,pending:null};
    entry.pending=new Promise(resolve=>{
      const reader=new FileReader();
      reader.onload=()=>{
        const result=String(reader.result||'');
        entry.data=result.slice(result.indexOf(',')+1).replace(/\s+/g,'');
        entry.size=base64Bytes(entry.data);
        renderAttachments();
        resolve();
      };
      reader.onerror=()=>{
        entry.error='unreadable';
        surfaceFailure(name+' could not be read');
        renderAttachments();
        resolve();
      };
      reader.readAsDataURL(file);
    });
    attachments.push(entry);
    renderAttachments();
  }
}
async function readyAttachments() {
  await Promise.all(attachments.map(entry=>entry.pending||Promise.resolve()));
  if (attachments.some(entry=>entry.error)) {
    surfaceFailure('Remove the unreadable attachment before sending');
    return null;
  }
  return attachments.filter(entry=>entry.data).map(entry=>({name:entry.name,data:entry.data}));
}
function hasAttachments() { return attachments.length>0; }

async function dispatch(text,options={}) {
  const value=String(text||'').trim();
  if (!value) return null;
  clearChoices();
  recall=null;
  const source=options.source||'typed';
  activeText=value;
  followLog=true;   // sending is a request to see the answer
  const user=options.userMessage||addMessage({
    role:'user', time:Date.now(), status:'complete',
    parts:[{type:'text',text:value}],
    meta:{source,project_id:activeProjectId()||undefined},
  });
  saveMessage(user);
  setBusy(true);
  setPhase('working');
  startTurn();
  turn.user=user;
  /* The request id keys this command's answer row; the daemon stamps it on every
   * event the command causes. */
  const requestId=uid('req')+'-'+Math.random().toString(36).slice(2,6);
  if (!turn.message) turn.request=requestId;
  setTurnLabel('Working');
  const pendingRequest={id:requestId,text:value,user,done:false,queued:false};
  request=pendingRequest;
  try {
    /* The turn resolves this project's directory and its own assistant
     * session, so a switch changes what "this project" means (BUG-088/090). */
    const params={text:value,project_id:activeProjectId()||undefined,request_id:requestId};
    if (Array.isArray(options.attachments)&&options.attachments.length) {
      params.attachments=options.attachments;
    }
    lastCommand={text:value,project_id:params.project_id,
      attachments:(params.attachments||[]).map(entry=>entry.name)};
    const result=await rpc('command',params);
    if (result&&result.queued===true) {
      pendingRequest.queued=true;
      user.status='queued';
      bump(user);
      queuedEvent({details:{position:result.position}});
      return user;
    }
    if (pendingRequest.done) return user;         // the event already owned the row
    const reply=result&&result.reply;
    if (reply&&typeof reply==='object') {
      replyDone({time:Date.now(),details:{request_id:requestId,...reply},message:reply.text||''});
    } else if (result&&result.stopped===true) closeTurn();
    return user;
  } catch (error) {
    if (!pendingRequest.done) failTurn(error.message,requestId);
    else record('warn','command rejected after the turn failed: '+error.message);
    return user;
  } finally {
    if (request===pendingRequest) request=null;
    updateStatus();
  }
}
function failTurn(reason,requestId) {
  const text=String(reason||'The assistant failed');
  lastFailure=text;
  const own=requestId?messageById.get(turnRowId(requestId)):null;
  if (own?.status==='failed') { finishRequest(requestId); return; }
  const open=own||turn?.message;
  let message=open;
  if (!open) {
    message=addMessage({
      id:requestId?turnRowId(requestId):undefined,
      role:'assistant', time:Date.now(), status:'failed', parts:[],
      meta:{project_id:activeProjectId()||undefined},
    });
    if (turn) turn.message=message;
  }
  markFailed(message,text);
  finishRequest(requestId);
  alarm(text);
  if (message) { saveMessage(message); bump(message); }
}

/* -- voice: display, confirm window, and the stop contract --------------- */

const VOICE_WORDS={
  off:'Mic off',listening:'Listening','waiting for window':'Waiting',transcribing:'Transcribing',
  confirming:'Confirming',queued:'Queued','loading speech':'Loading voice',stopped:'Stopped',
  discarded:'Mic off','deciding':'Working','listening for your answer':'Your answer',
};
function voiceWord(status) {
  const value=String(status||'off');
  if (VOICE_WORDS[value]) return VOICE_WORDS[value];
  if (/busy/i.test(value)) return 'Busy';
  const first=value.split(/[;,·]/)[0].trim();
  return first?first.charAt(0).toUpperCase()+first.slice(1):'Mic off';
}
function voiceNotes(voice) {
  const bits=[];
  const language=String(snapshot?.settings?.speech?.language||'en').trim()||'en';
  bits.push(language);
  if (typeof voice.confidence==='number') bits.push(Math.round(voice.confidence*100)+'%');
  if (voice.source) bits.push(String(voice.source));
  if (voice.unsure) bits.push('low confidence');
  if (voice.truncated) bits.push('recording limit reached — text may be incomplete');
  if (voice.gap) bits.push('audio gap');
  return bits.join(' · ');
}
const confirmState={active:false,transcript:'',deadline:0,timer:null,filled:false};
function setVoiceBanner(options) {
  const banner=element('voice-banner');
  if (!banner) return;
  if (!options||!options.text) { banner.hidden=true; return; }
  banner.hidden=false;
  element('voice-source').textContent=options.notes||'';
  element('voice-text').textContent=options.text;
  element('voice-count').textContent=options.countdown||'';
  element('voice-use').hidden=options.actions!==true;
  element('voice-edit').hidden=options.actions!==true;
}
function beginConfirm(voice) {
  const text=String(voice.transcript||'').trim();
  if (!text) return;
  const seconds=Number(voice.confirm_seconds)||1.5;
  if (confirmState.active&&confirmState.transcript===text&&Date.now()<confirmState.deadline) return;
  confirmState.active=true;
  confirmState.transcript=text;
  confirmState.deadline=Date.now()+seconds*1000;
  const input=commandInput();
  if (input&&(!input.value.trim()||input.value===text)) { input.value=text; confirmState.filled=true; }
  setVoiceBanner({text,notes:voiceNotes(voice),countdown:seconds.toFixed(1)+'s',actions:true});
  clearInterval(confirmState.timer);
  confirmState.timer=setInterval(tickConfirm,100);
  tickConfirm();
}
function tickConfirm() {
  if (!confirmState.active) { clearInterval(confirmState.timer); confirmState.timer=null; return; }
  const left=Math.max(0,(confirmState.deadline-Date.now())/1000);
  const count=element('voice-count');
  if (count) count.textContent=left.toFixed(1)+'s';
  if (left<=0) {
    /* The worker owns the dispatch at the end of its hold; the composer returns to
       the user instead of sending the transcript a second time. */
    finishConfirm({clear:true});
  }
}
function finishConfirm(options={}) {
  clearInterval(confirmState.timer);
  confirmState.timer=null;
  const wasActive=confirmState.active;
  confirmState.active=false;
  setVoiceBanner(null);
  if (wasActive&&confirmState.filled&&options.clear) {
    const input=commandInput();
    if (input&&input.value===confirmState.transcript) input.value='';
  }
  confirmState.filled=false;
}
/* BUG-056: the worker dispatches the held transcript when its confirm window runs
 * out, so Enter has to ask the daemon who won first. `claimed:true` means the UI
 * owns the send; `claimed:false` means the command already left, and sending a
 * second one would run it twice. Escape withdraws the hold instead. */
async function useConfirm() {
  if (!confirmState.active) { submitForm(); return; }
  const input=commandInput();
  const edited=input?input.value.trim():'';
  const text=edited||confirmState.transcript;
  finishConfirm({clear:true});
  let claim;
  try {
    claim=await rpc('voice_confirm',{accept:true});
  } catch (error) {
    /* Nothing was sent and the claim is unknown, so keep the draft for a retry. */
    if (text) { const field=commandInput(); if (field&&!field.value.trim()) field.value=text; }
    surfaceFailure('Could not confirm the transcript: '+error.message);
    return;
  }
  if (claim&&claim.claimed===false) {
    toast('That transcript was already sent by the voice worker');
    return;
  }
  if (text) dispatch(text,{source:'voice'});
}
function editConfirm() {
  const input=commandInput();
  const wasActive=confirmState.active;
  finishConfirm();
  if (wasActive) rpc('voice_confirm',{accept:false}).catch(error=>record('warn','voice hold cancel failed: '+error.message));
  if (input) { input.focus(); input.setSelectionRange(input.value.length,input.value.length); }
  toast('Edit the transcript, then press Enter to send it');
}
function applyVoice(voice) {
  const status=String(voice.status||'off');
  const pill=element('voice-pill');
  if (pill) {
    pill.textContent=voiceWord(status);
    pill.dataset.status=status;
    pill.hidden=status==='off'||status==='discarded';
    pill.title='Voice: '+status+(voice.wake_word?' · say “'+voice.wake_word+'”':'');
  }
  if (status==='confirming'&&voice.transcript) beginConfirm(voice);
  else if (confirmState.active) finishConfirm({clear:true});
  if (status==='transcribing'&&voice.partial) {
    setVoiceBanner({text:String(voice.partial),notes:voiceNotes(voice)+' · transcribing',countdown:'',actions:false});
  } else if (status!=='confirming'&&status!=='transcribing') {
    const outcome=/^sent to |session busy|session unreachable|^queued$/.test(status)?status:'';
    if (outcome) setVoiceBanner({text:outcome,notes:voiceNotes(voice),countdown:'',actions:false});
    else if (!confirmState.active&&status!=='transcribing') setVoiceBanner(null);
  }
  const note=element('mic-note');
  if (note) note.textContent=micExplanation(status);
  const speech=element('chief-status');
  if (speech) {
    const language=String(snapshot?.settings?.speech?.language||'en').trim()||'en';
    speech.textContent=language+' · wake word is English-only';
    speech.title='Speech language: '+language+'. The wake-word model is English only; the transcript language follows the setting.';
  }
}
function micExplanation(status) {
  if (host.isolated===true) return 'The microphone is disabled in isolated mode: this window runs against a private daemon with no hardware access.';
  if (!snapshot?.settings?.microphone) return 'The microphone is off. Enable it to speak; typing works either way.';
  if (status==='off') return 'Microphone is enabled but the voice worker is not running yet.';
  return 'Say “'+String(snapshot?.voice?.wake_word||'Hey Navi')+'”, then your request.';
}

function setBusy(value) {
  busy=!!value;
  const stop=element('stop');
  if (stop) stop.hidden=!busy;
  updateStatus();
}

/* ---------------- slash command suggestions ---------------- */

/* The catalog is a cache with a 60 s TTL: it used to be fetched once and kept
 * forever, including a failure, so suggestions never recovered (BUG-017). A failed
 * read is never cached, and `oc_new` invalidates the whole set. */
const CATALOG_TTL=60000;
const catalogs={commands:{list:null,at:0},models:{list:null,at:0},agents:{list:null,at:0}};
let slashCommands=null;
async function catalog(name,op) {
  const entry=catalogs[name];
  if (entry.list&&Date.now()-entry.at<CATALOG_TTL) return entry.list;
  const list=await rpc(op);
  /* A gateway error arrives as a result object, not as a rejection: an error is not
   * an empty catalog, and it must not be cached either (BUG-017). */
  if (!Array.isArray(list)) {
    throw new Error(String(list&&list.error||'the catalog returned no list'));
  }
  entry.list=list;
  entry.at=Date.now();
  if (name==='commands') {
    slashCommands=entry.list.map(item=>({name:item.name||'',description:item.description||''})).filter(item=>item.name);
  }
  return entry.list;
}
function invalidateCatalogs() {
  for (const entry of Object.values(catalogs)) { entry.list=null; entry.at=0; }
  slashCommands=null;
}
/* A failure is never cached, but it is not retried on every keystroke either:
 * without the assistant, typing "/progress" would make nine failing calls. */
const CATALOG_RETRY=5000;
let commandsFailedAt=0;
async function commandCatalog() {
  if (slashCommands&&Date.now()-catalogs.commands.at<CATALOG_TTL) return slashCommands;
  if (Date.now()-commandsFailedAt<CATALOG_RETRY) return slashCommands||[];
  try { await catalog('commands','oc_commands'); }
  catch { slashCommands=null; commandsFailedAt=Date.now(); }
  return slashCommands||[];
}
const slashList=()=>element('slash-list'), commandInput=()=>element('command');
let slashIndex=0, slashMatches=[], slashPrefix='';
function hideSlash(){ slashList().hidden=true; slashMatches=[]; }
function applySlash(command) {
  const input=commandInput(), raw=input.value;
  const rest=raw.slice(1).split(/\s/).slice(1).join(' ');
  input.value='/' + command + (rest?' '+rest:' ');
  hideSlash();
}
function pickSlash(index) {
  if (slashMatches[index]) applySlash(slashMatches[index].name);
}
/* Navi's own commands, answered by the daemon's fast path without a model;
 * the assistant's commands follow them when its catalog is available. */
const NAVI_COMMANDS=[
  {name:'help',description:'What Navi does without a model'},
  {name:'sessions',description:'What each open harness is doing'},
  {name:'progress',description:'How far along the active project is, and its parts'},
  {name:'assess',description:'Scan the project and ask a harness to estimate each part'},
  {name:'open',description:'Open a harness in the active project: /open claude code'},
  {name:'mark',description:'Set a task’s state: /mark barge-in done'},
  {name:'set',description:'Set a part’s percent: /set voice to 80%'},
];
function renderSlash(prefix) {
  slashPrefix=prefix;
  const own=new Set(NAVI_COMMANDS.map(command=>command.name));
  slashMatches=[...NAVI_COMMANDS,...(slashCommands||[]).filter(command=>!own.has(command.name))]
    .filter(c=>!prefix||c.name.startsWith(prefix)).slice(0,12);
  if (!slashMatches.length) { hideSlash(); return; }
  slashIndex=Math.min(slashIndex,slashMatches.length-1);
  const list=slashList(); list.replaceChildren();
  slashMatches.forEach((command,index)=>{
    const item=document.createElement('li');
    item.id='slash-'+index; item.setAttribute('role','option');
    if (index===slashIndex) item.classList.add('active');
    const name=document.createElement('span'); name.className='slash-name'; name.textContent='/'+command.name;
    const hint=document.createElement('span'); hint.className='slash-hint'; hint.textContent=command.description;
    item.append(name,hint);
    item.onmousedown=event=>{ event.preventDefault(); applySlash(command.name); };
    list.append(item);
  });
  list.hidden=false;
}
async function updateSlash() {
  const input=commandInput();
  if (!input.value.startsWith('/')) { hideSlash(); return; }
  const token=input.value.slice(1).split(/\s/)[0];
  if (slashCommands===null) await commandCatalog();
  if (input.value.startsWith('/')) renderSlash(token);
}
commandInput().addEventListener('input',updateSlash);
commandInput().addEventListener('keydown',event=>{
  if (slashList().hidden) return;
  if (event.key==='ArrowDown') { slashIndex=(slashIndex+1)%slashMatches.length; renderSlash(slashPrefix); }
  else if (event.key==='ArrowUp') { slashIndex=(slashIndex-1+slashMatches.length)%slashMatches.length; renderSlash(slashPrefix); }
  else if (event.key==='Tab'||(event.key==='Enter'&&commandInput().value.slice(1).split(/\s/)[0]!==slashMatches[slashIndex]?.name)) {
    event.preventDefault(); pickSlash(slashIndex);
  }
  else if (event.key==='Escape') { event.preventDefault(); hideSlash(); }
});
commandInput().addEventListener('blur',()=>setTimeout(hideSlash,150));

/* ArrowUp/ArrowDown from an empty box walk this project's earlier commands, newest
 * first, like a shell; typing or sending ends the walk. */
let recall=null;   // {index, draft} while walking
function sentCommands() {
  const seen=new Set(), list=[];
  for (let index=messages.length-1; index>=0&&list.length<50; index--) {
    const message=messages[index];
    if (message.role!=='user') continue;
    const text=messageText(message).trim();
    if (!text||seen.has(text)) continue;
    seen.add(text);
    list.push(text);
  }
  return list;
}
commandInput().addEventListener('keydown',event=>{
  if (event.key!=='ArrowUp'&&event.key!=='ArrowDown') return;
  if (!slashList().hidden||confirmState.active||event.altKey||event.ctrlKey||event.metaKey||event.shiftKey) return;
  const input=commandInput();
  if (!recall&&(event.key==='ArrowDown'||input.value.trim())) return;
  const list=sentCommands();
  if (!list.length) return;
  event.preventDefault();
  if (!recall) recall={index:-1,draft:input.value};
  recall.index=Math.max(-1,Math.min(list.length-1,recall.index+(event.key==='ArrowUp'?1:-1)));
  input.value=recall.index<0?recall.draft:list[recall.index];
  if (recall.index<0) recall=null;
  input.setSelectionRange(input.value.length,input.value.length);
});
commandInput().addEventListener('input',()=>{ recall=null; });

/* ---------------- status feed ---------------- */

const TURN_EVENTS=new Set(['working','thinking','reasoning','reply_delta','reply','tool_use',
  'approval','stopped','command']);
/* The request of the turn the daemon reports as running, from the newest turn
 * opening in the feed; kept across pages because a turn can straddle them. */
let replayOpen=null;
function openReplayRequest(payload,events) {
  if (payload.busy!==true) { replayOpen=null; return null; }
  for (let index=events.length-1; index>=0; index--) {
    const kind=events[index]?.kind;
    if (kind==='working'||kind==='thinking') { replayOpen=requestOf(events[index]); break; }
  }
  return replayOpen;
}

/* `update` is the single entry point for the daemon payload: settings, sessions,
 * voice and every event. The host can also push a payload straight in (`navi.push`)
 * and a partial payload is merged over the last snapshot instead of replacing it. */
/* A new commit changes "N commits since" without a progress event, so a moved
 * HEAD refreshes the graph (active project) and the switcher's overview. */
const seenHeads=new Map();
function noteHeads(projects) {
  if (!Array.isArray(projects)) return;
  let moved=false;
  for (const project of projects) {
    if (!project?.id) continue;
    const id=String(project.id), head=String(project.git_head||'');
    if (seenHeads.has(id)&&seenHeads.get(id)!==head) {
      moved=true;
      if (id===activeProjectId()) scheduleGraph(600);
    }
    seenHeads.set(id,head);
  }
  if (moved) scheduleOverview(1200);
}
/* Voice commands carry no project, so the daemon falls back to the one this station
 * shows: reported once per daemon boot; switchProject reports later changes. */
let scopedBoot=null;
function reportScope() {
  const id=activeProjectId();
  if (!id||!bootId||scopedBoot===bootId) return;
  scopedBoot=bootId;
  rpc('assistant_scope',{project_id:id})
    .then(scope=>{ assistantScope=scope; updateProjectLine(); })
    .catch(error=>record('warn','assistant scope failed: '+error.message));
}
function update(value) {
  if (!value||typeof value!=='object') return;
  /* Only a payload that carries `settings` is the full snapshot; a sectioned push or
   * an event-only page is merged over the last one, so a missing key keeps its value. */
  const full=!!(value.settings&&typeof value.settings==='object');
  const payload=full?value:{...(snapshot||{}),...value};
  snapshot=payload;
  polledOnce=true;
  reconcileFeed(payload);
  if (typeof payload.busy==='boolean') applyServerBusy(payload.busy);
  surfaceSettings(payload);
  renderSessions(payload.sessions||[]);
  renderProjectSwitcher();
  updateProjectLine();
  syncProgressScope();
  noteHeads(payload.projects);
  reportScope();
  applyVoice(payload.voice||{});
  const events=Array.isArray(payload.events)?payload.events:[];
  const openRequest=openReplayRequest(payload,events);
  for (const event of events) {
    if (!event||typeof event!=='object') continue;
    /* De-duplicate by sequence number, never by text: two identical replies with
     * different seqs are two messages, and one seq delivered twice is one (BUG-046). */
    if (event.seq!==undefined&&event.seq!==null) {
      if (seenSeqs.has(event.seq)) continue;
      seenSeqs.add(event.seq);
      seenOrder.push(event.seq);
      while (seenOrder.length>SEEN_LIMIT) seenSeqs.delete(seenOrder.shift());
      seq=Math.max(seq,Number(event.seq)||0);
    }
    if (Number(event.time)<replayBefore) event.replayed=true;
    /* Finished turns come back from the message store; replaying their events
     * drew every past answer again on each boot. Only the turn the daemon is
     * still running is rebuilt from the feed. */
    if (event.replayed&&TURN_EVENTS.has(event.kind)&&!(openRequest&&requestOf(event)===openRequest)) continue;
    /* An event's project_id attributes the row; it never moves the active
     * project, which only the switcher and Ctrl+1..9 change (BUG-090). */
    try { handleEvent(event); }
    catch (error) { record('warn',`event ${event.kind} failed: ${error.message}`); }
  }
  /* A coalesced turn payload replaces the live row's text; `done` ends the turn
   * without waiting for the next poll (BUG-015/098). */
  if (payload.turn_state) {
    try { applyTurnState(payload.turn_state); }
    catch (error) { record('warn','turn_state failed: '+error.message); }
  }
  updateStatus();
  hydrateOnce();
  /* BUG-015: a page with more events behind it is fetched immediately. */
  if (payload.has_more===true) {
    clearTimeout(pollTimer);
    pollTimer=setTimeout(poll,40);
  }
}

/* `status.busy` is the daemon's own view of the turn and it is authoritative — but a
 * push prepared before our submit opened the turn must not clear the row we just made,
 * so a submit in flight wins until its reply lands. */
function applyServerBusy(serverBusy) {
  if (request) return;
  if (serverBusy===false&&turn&&turn.done!==true) {
    turn.done=true;
    const message=turn.message;
    if (message&&message.status==='streaming') {
      message.status='complete';
      bump(message);
    }
  }
  setBusy(serverBusy===true);
}

/* `turn_state` is the producer's coalesced view of the open turn:
 * `{turn,text,done}`. It replaces (never appends) the live row and only applies
 * when it describes the row that is actually on screen. */
function applyTurnState(state) {
  if (!state||typeof state!=='object') return;
  const text=typeof state.text==='string'?state.text:null;
  const turnId=state.turn===undefined||state.turn===null?null:String(state.turn);
  const message=turn?.message||null;
  const known=message&&message.turn!==undefined&&message.turn!==null?String(message.turn):null;
  /* An empty, anonymous payload means "nothing is running": it must not rewrite or
   * close a row the event feed opened. A payload describes a turn only when it names
   * one or carries text. */
  const describes=turnId!==null||(text!==null&&text.trim()!=='');
  if (!describes) { updateStatus(); return; }
  /* A row with no bound turn id only adopts a named *running* turn: a stale finished
   * payload (the previous answer) must never fill or close a fresh row. */
  const adopt=!known&&!!turnId&&state.done!==true;
  const matches=known?(!turnId||known===turnId):adopt;
  if (message&&text!==null&&matches) {
    const part=lastTextPart(message,true);
    part.text=text;
    if (!message.turn&&turnId) message.turn=turnId;
    message.status=state.done===true?'complete':'streaming';
    bump(message);
  }
  if (matches) {
    if (state.done===true) {
      if (turn) turn.done=true;
      setBusy(false);
      setPhase('idle');
    } else if (!turn||turn.done!==true) {
      setBusy(true);
      setPhase('working');
    }
  }
  updateStatus();
}

/* BUG-099: a restarted daemon, or a cursor older than the retained ring, means the
 * client is missing events. Reset the cursor and ask for one full snapshot. */
let bootId=null, resyncing=false;
function reconcileFeed(value) {
  let reset=false;
  if (typeof value.boot_id==='string'&&value.boot_id) {
    if (bootId&&bootId!==value.boot_id) reset=true;
    bootId=value.boot_id;
  }
  const oldest=Number(value.events_from);
  if (Number.isFinite(oldest)&&oldest>1&&seq>0&&seq<oldest-1) reset=true;
  if (!reset) return;
  seq=0;
  replayBefore=Date.now()-2000;
  seenSeqs.clear(); seenOrder.length=0;
  if (turn) {
    const open=turn.message;
    if (open&&open.status==='streaming') { open.status='stopped'; bump(open); }
    closeTurn();
  }
  if (resyncing) return;
  resyncing=true;
  setTimeout(()=>{ resyncing=false; poll(); },0);
}

function surfaceSettings(value) {
  const settings=value.settings||{};
  const status=String(value.voice?.status||'off');
  const mic=element('microphone');
  if (mic) {
    mic.disabled=value.isolated===true;
    mic.setAttribute('aria-pressed',String(!!settings.microphone));
    mic.firstElementChild.textContent=settings.microphone?'Mic on':'Mic off';
    mic.title=micExplanation(status);
  }
  const emptyMic=element('empty-mic');
  if (emptyMic) emptyMic.disabled=value.isolated===true||!!settings.microphone;
  const launch=element('empty-launch');
  if (launch) {
    const profile=preferredProfile(settings.profiles||[]);
    launch.hidden=!profile||!projectList().length;
    if (profile) launch.textContent='Open '+(profile.name||profile.id);
  }
  const assess=element('empty-assess');
  if (assess) assess.disabled=!projectList().length;
  const note=element('mic-note');
  if (note) note.textContent=micExplanation(status);
  const switcher=element('model-switcher');
  if (switcher) {
    const reference=String(settings.assistant?.model||'');
    switcher.textContent=shortModel(reference)||'Model';
    switcher.title=reference?'Assistant model: '+reference:'No assistant model is saved';
  }
  const speechNote=element('speech-note');
  if (speechNote) {
    const language=String(settings.speech?.language||'en').trim()||'en';
    speechNote.textContent='Speech language: '+language+' · the wake-word model is English-only.';
  }
}

/* Progress follows the project list and the active project: a new project gets
 * its switcher percent, and a changed selection loads its graph once. */
let progressScope=null;
function syncProgressScope() {
  const list=projectList().map(project=>project.id).join('|');
  if (list!==progressScope) { progressScope=list; loadOverview(); }
  const active=activeProjectId()||null;
  if (graphRequested!==active) loadGraph();
}
let hydratedChat=false;
function hydrateOnce() {
  if (hydratedChat||!snapshot?.settings) return;
  hydratedChat=true;
  loadHistory(HISTORY_START);
}
let polls=0, pollFailures=0, offline=false, backoff=700, pollTimer=null, polling=false, daemon=null;
/* Isolated-only: the GUI workflow fabricates a turn, and the daemon's own idle
 * payloads would (correctly) end it. Pausing the feed is the test's lever. */
let pollPaused=false;

async function refreshDaemon() {
  try { daemon=await rpc('ping'); } catch { /* diagnostics report "unknown" */ }
  return daemon;
}
async function poll() {
  if (pollPaused) { pollTimer=null; return; }
  if (polling) return;  // one request in flight, whoever calls us
  polling=true; pollTimer=null;
  try {
    /* A status reply is the health signal: the attention badge is a transient
     * projection, so a successful poll clears it while the error row stays in the
     * transcript (BUG-036). */
    needsAttention=false;
    update(await rpc('status',{gui:true,since:seq}));
    polls++;
    pollFailures=0; backoff=700;
    if (offline) { offline=false; toast('Reconnected to Navi'); updateStatus(); }
    else updateStatus();
    if (!daemon) refreshDaemon();
  } catch (error) {
    pollFailures++;
    /* One toast per transition, not per failure: a daemon restart must not
     * spray the chat with identical errors. */
    if (!offline && pollFailures>=3) {
      offline=true; lastFailure=error.message;
      updateStatus(); toast(error.message);
    }
    backoff=Math.min(15000, backoff*2);
  } finally {
    // Always re-arm: a lost reply must never stop the status feed.
    polling=false;
    clearTimeout(pollTimer);
    pollTimer=setTimeout(poll, backoff);
  }
}

/* One submit path for typing, voice and the empty-state prompt. */
element('chat-form').onsubmit=event=>{
  event.preventDefault();
  submitForm();
};
async function submitForm() {
  const input=commandInput();
  const text=input?input.value.trim():'';
  if (!text&&!hasAttachments()) return null;
  /* A Send click during the confirm window is the same race as Enter, so it goes
   * through the same claim instead of dispatching behind the worker's back. */
  if (confirmState.active) return useConfirm();
  const files=await readyAttachments();
  if (files===null) return null;
  if (!text&&!files.length) return null;
  hideSlash();
  input.value='';
  if (files.length) { attachments=[]; renderAttachments(); }
  return dispatch(text,{source:'typed',attachments:files});
}
const attachInput=element('attach-input');
const attachButton=element('attach');
if (attachButton&&attachInput) {
  attachButton.onclick=()=>attachInput.click();
  attachInput.onchange=()=>{
    const files=attachInput.files;
    attachInput.value='';
    attachFiles(files);
  };
}

element('stop').onclick=async()=>{
  try {
    const result=await rpc('cancel');
    /* BUG-007: an unconfirmed interrupt is reported, never assumed. The daemon
     * emits the same warning; the failure surface keeps it to one row. */
    if (result&&result.interrupted===false) {
      warningRow('Assistant turn may still be running: OpenCode did not confirm the interrupt');
    }
  } catch (error) { surfaceFailure(error.message); }
  setBusy(false);
  setPhase('idle');
  updateStatus();
};

function toggleMicrophone() {
  if (!snapshot) return;
  if (snapshot.isolated===true) { toast('The microphone is disabled in isolated mode'); return; }
  const settings=snapshot.settings||{};
  const next={...settings,microphone:!settings.microphone};
  rpc('settings',next).then(()=>{
    snapshot.settings=next;
    surfaceSettings(snapshot);
    if (next.microphone) rpc('voice_start').catch(()=>null);
  }).catch(error=>surfaceFailure(error.message));
}
element('microphone').onclick=toggleMicrophone;
element('voice-pill').onclick=toggleMicrophone;
element('voice-use').onclick=()=>useConfirm();
element('voice-edit').onclick=()=>editConfirm();
element('empty-mic').onclick=toggleMicrophone;
element('empty-session').onclick=()=>openNewDialog();
element('empty-launch').onclick=()=>{ const profile=preferredProfile(); if (profile) quickLaunch(profile); };
element('empty-assess').onclick=()=>assessProject();
element('empty-settings').onclick=()=>openSettings();

/* The reader owns the log: nothing auto-scrolls unless the view is already at the
 * bottom, and a pill offers to jump when content lands below it (BUG-047). */
chatLog().addEventListener('scroll',()=>{
  const log=chatLog();
  if (Math.abs(log.scrollTop-ownScrollTop)>1) followLog=atBottom(log);
  updatePill(log);
  if (log.scrollTop<200) loadOlder();
  if (messages.length>RENDER_WINDOW*2+1) chatChanged();
});
element('chat-pill').onclick=()=>{
  const log=chatLog();
  if (!log) return;
  pinLog(log);
  updatePill(log);
};
/* The log's own box changes when the toast or the voice banner shows or hides,
 * or the panel resizes; a following reader stays on the newest row. */
if (typeof ResizeObserver==='function') {
  new ResizeObserver(()=>{
    const log=chatLog();
    if (followLog) pinLog(log);
    updatePill(log);
  }).observe(chatLog());
}
/* Enter accepts a voice transcript; Escape turns it into an editable draft. */
commandInput().addEventListener('keydown',event=>{
  if (!confirmState.active) return;
  if (event.key==='Enter'&&!event.shiftKey) { event.preventDefault(); useConfirm(); }
  else if (event.key==='Escape') { event.preventDefault(); editConfirm(); }
});
updateEmptyState();
updateStatus();

/* ---------------- harness sessions + terminal ---------------- */

let term=null, termSession=null;
function ensureTerm() {
  if (term) return term;
  term=new Terminal({cursorBlink:true,convertEol:true,allowProposedApi:true,
    fontFamily:'ui-monospace,Menlo,Consolas,monospace',fontSize:12,
    theme:{background:'#030a2b',foreground:'#d9eaff',cursor:'#8dffe0',
      selectionBackground:'#3150a8aa'}});
  term.onData(data=>{
    send({type:'binary',data:btoa(data)});
  });
  term.open(element('terminal-host'));
  return term;
}
function cellSize() {
  const dims=term?._coreService?._renderService?.dimensions?.css?.cell;
  if (dims?.width&&dims?.height) return dims;
  const probe=document.createElement('span');
  probe.style.cssText='position:absolute;visibility:hidden;white-space:pre;font:12px ui-monospace,Menlo,Consolas,monospace';
  probe.textContent='M'.repeat(50);
  document.body.append(probe);
  const width=probe.getBoundingClientRect().width/50||7.2, height=parseFloat(getComputedStyle(probe).lineHeight)||16;
  probe.remove();
  return {width,height};
}
function fitTerm() {
  if (!term||dockMode()==='closed'||element('terminal-pane').hidden) return;
  const host=element('terminal-host'), box=host.getBoundingClientRect(), cell=cellSize();
  const cols=Math.max(20,Math.floor((box.width-12)/cell.width));
  const rows=Math.max(6,Math.floor((box.height-6)/cell.height));
  if (cols!==term.cols||rows!==term.rows) term.resize(cols,rows);
  send({type:'resize',cols,rows});
}
function openTerminal(session) {
  if (!session?.running) { toast('Session is not running'); return; }
  ensureTerm();
  setDock(preferredDock());
  termSession=session.id;
  syncTerminalTitle(session);
  term.clear();
  send({type:'attach',id:session.id});
  /* Opening a session makes it the active one, which is what a spoken "tell this
   * one…" resolves against and what the badge in the strip reports (BUG-065). */
  rpc('select',{id:session.id}).catch(error=>record('warn','select failed: '+error.message));
  renderSessions(snapshot?.sessions||[]);
  scheduleFit();
  requestAnimationFrame(()=>{ fitTerm(); term.focus(); });
}
/* The header names the session the way its chip does ("Terminal #2"), then
 * the harness only when the name does not already say it, then the project:
 * "Terminal · Terminal" told the user nothing the chip had not. */
function syncTerminalTitle(session) {
  if (!session||session.id!==termSession) return;
  const name=sessionLabels.get(session.id)||String(session.name||'');
  const profile=profileLabel(session.profile);
  const project=projectList().find(row=>row.id===session.project_id)?.name;
  const parts=[name,profile.toLowerCase()!==String(session.name||'').toLowerCase()?profile:null,project];
  /* A session that ended under the pane says so, and stops taking input. */
  const ended=session.running!==true;
  if (ended) parts.push('exited');
  element('terminal-title').textContent=parts.filter(Boolean).join(' · ')||'Terminal';
  for (const id of ['terminal-prompt','terminal-stop']) {
    const control=element(id);
    if (control) control.disabled=ended;
  }
  element('terminal-prompt').placeholder=ended?'This session has ended':'Prompt this harness…';
  /* Send turns into Reopen: the same restart a finished chip offers. */
  const send=element('terminal-send');
  send.textContent=ended?'Reopen':'Send';
  send.dataset.reopen=String(ended&&!session.replaced_by);
  send.disabled=ended&&!!session.replaced_by;
}
function closeTerminal() {
  setDock('closed');
  termSession=null;
  send({type:'attach'});
  commandInput().focus();
}
element('terminal-close').onclick=closeTerminal;
/* BUG-082: Stop reaches the harness itself, not just the assistant turn. The
 * daemon sends the profile's interrupt keys and records the state event. */
element('terminal-stop').onclick=async()=>{
  const id=termSession;
  if (!id) return;
  try {
    const result=await rpc('session_interrupt',{id});
    toast(result&&result.killed?'Harness session stopped':'Interrupt sent to the harness');
  } catch (error) { surfaceFailure(error.message); }
};
element('terminal-send').onclick=()=>{
  if (element('terminal-send').dataset.reopen==='true') {
    const session=(snapshot?.sessions||[]).find(row=>row.id===termSession);
    if (session) reopenSession(session,sessionLabels.get(session.id)||session.name);
    return;
  }
  const input=element('terminal-prompt'), text=input.value.trim();
  if (!text||!termSession) return;
  input.value='';
  rpc('prompt',{id:termSession,text}).catch(error=>toast(error.message));
};
element('terminal-prompt').addEventListener('keydown',event=>{ if (event.key==='Enter') element('terminal-send').click(); });

/* ---------------- layout: phase, dock, graph, and the stage frame ---------------- */

/* The station owns the four layout facts the CSS cannot derive: the turn phase,
 * which dock mode is live, how far the splitter was dragged, and whether the
 * graph drawer is open. The scene is told the stage cell and the dim level so
 * the artwork is framed by its own cell instead of the window. */

let phase='idle';
function setPhase(value) {
  /* One call per transition: the scene glitches on the working edge, so a
   * repeated setPhase('working') on every event would tear the stage per poll. */
  const next=value==='working'||value==='error'?value:'idle';
  if (next===phase) return;
  phase=next;
  window.naviScene?.setPhase?.(next);
}

/* Dock state is remembered under its own key: window_state belongs to the host
 * and holds window geometry, so the two records must never collide. */
const DOCK_KEY='navi.dock';
const dockPane=()=>element('terminal-pane');
const splitter=()=>element('dock-splitter');
/* Seed from storage so a ratio remembered for the other orientation survives the
 * next write. The shell starts closed in the DOM, not only in the CSS. */
const rememberedDock=readDock();
let dockRatios=rememberedDock.ratios&&typeof rememberedDock.ratios==='object'?rememberedDock.ratios:{};
let lastDockMode=rememberedDock.mode==='bottom'?'bottom':'right';
let fitTimer=null, graphOpen=false;
document.body.dataset.dock='closed';
document.body.dataset.graph='closed';

function dockMode() { return document.body.dataset.dock||'closed'; }

function readDock() {
  try {
    const value=JSON.parse(localStorage.getItem(DOCK_KEY)||'{}');
    return value&&typeof value==='object'?value:{};
  } catch { return {}; }
}
function writeDock() {
  try { localStorage.setItem(DOCK_KEY,JSON.stringify({mode:lastDockMode,ratios:dockRatios})); }
  catch { /* storage can be unavailable; the dock still works for this session */ }
}
function rememberDock(mode, ratio) {
  if (mode==='right'||mode==='bottom') dockRatios={...dockRatios,[mode]:ratio};
  clearTimeout(rememberDock.timer);
  rememberDock.timer=setTimeout(writeDock,250);
}

/* The splitter axis follows the cell, not the stored mode: below 1200 px the
 * CSS moves the dock under the stage and the splitter turns horizontal. */
function dockAxis(mode=dockMode()) {
  if (mode==='bottom') return 'y';
  return matchMedia('(max-width:1199px)').matches?'y':'x';
}
function dockExtent(axis) { return axis==='y'?innerHeight:innerWidth; }
function clampDock(axis, size) {
  /* Both sides keep 120 px, so neither the stage nor the dock can collapse. */
  const limit=Math.max(120,dockExtent(axis)-120);
  return Math.round(Math.min(limit,Math.max(120,size)));
}
function applyDockSize(axis, size, mode=dockMode()) {
  const value=clampDock(axis,size);
  document.body.style.setProperty(axis==='y'?'--dock-h':'--dock',value+'px');
  rememberDock(mode,value/dockExtent(axis));
  return value;
}
function preferredDock() {
  const mode=readDock().mode;
  /* Open to the side only where the CSS can hold a second column. */
  if ((mode==='right'||mode==='bottom')&&(mode==='right')===(innerWidth>=1200)) return mode;
  return innerWidth>=1200?'right':'bottom';
}

function setDock(mode) {
  const pane=dockPane();
  if (!pane) return dockMode();
  const requested=mode==='right'||mode==='bottom'||mode==='max'?mode:'closed';
  /* Where the CSS can hold a second column the dock opens to the right; below
   * 1200 px the same request becomes an under-stage dock. */
  const next=requested==='closed'?'closed':requested==='max'?'max':(innerWidth>=1200?'right':'bottom');
  document.body.dataset.dock=next;
  pane.hidden=next==='closed';  // the attribute still hides the card if the CSS is lost
  if (next==='right'||next==='bottom') {
    lastDockMode=next;
    const axis=dockAxis(next), stored=readDock().ratios?.[next];
    if (typeof stored==='number') applyDockSize(axis,stored*dockExtent(axis),next);
    else document.body.style.removeProperty(next==='right'?'--dock':'--dock-h');
  }
  splitter()?.setAttribute('aria-orientation',dockAxis()==='y'?'horizontal':'vertical');
  writeDock();
  syncFrame();
  scheduleFit();
  return next;
}

/* A resize can cross the 1200 px breakpoint, which moves the dock under the
 * stage; keep `data-dock` honest so the stored mode always describes the cell. */
function reconcileDock() {
  const mode=dockMode();
  if (mode==='right'&&innerWidth<1200) setDock('bottom');
  else if (mode==='bottom'&&innerWidth>=1200) setDock('right');
}

let drag=null;
function startDrag(event) {
  if (event.button!==0) return;
  const mode=dockMode();
  if (mode!=='right'&&mode!=='bottom') return;
  event.preventDefault();
  const axis=dockAxis(mode), box=dockPane().getBoundingClientRect();
  drag={axis,mode,pointer:event.pointerId,
    origin:axis==='y'?event.clientY:event.clientX,
    size:axis==='y'?box.height:box.width};
  try { splitter().setPointerCapture?.(event.pointerId); }
  catch { /* a pointer that is already gone cannot be captured */ }
  document.body.classList.add('resizing');
}
function moveDrag(event) {
  if (!drag||event.pointerId!==drag.pointer) return;
  const now=drag.axis==='y'?event.clientY:event.clientX;
  /* Dragging away from the stage grows the dock on both axes. */
  applyDockSize(drag.axis,drag.size-(now-drag.origin),drag.mode);
  scheduleFit();
}
function endDrag(event) {
  if (!drag) return;
  try { if (event) splitter()?.releasePointerCapture?.(drag.pointer); }
  catch { /* the capture is dropped with the pointer anyway */ }
  drag=null;
  document.body.classList.remove('resizing');
  writeDock();
  fitTerm();
}
function toggleDockMax() {
  const mode=dockMode();
  if (mode==='max') setDock(lastDockMode);
  else if (mode==='closed') setDock(preferredDock());
  else setDock('max');
}
function nudgeDock(event) {
  const mode=dockMode();
  if (mode!=='right'&&mode!=='bottom') return;
  const axis=dockAxis(mode), step=event.shiftKey?64:16;
  const box=dockPane().getBoundingClientRect();
  const size=axis==='y'?box.height:box.width;
  const grow=axis==='y'?event.key==='ArrowUp':event.key==='ArrowLeft';
  const shrink=axis==='y'?event.key==='ArrowDown':event.key==='ArrowRight';
  if (!grow&&!shrink) return;
  event.preventDefault();
  applyDockSize(axis,size+(grow?step:-step),mode);
  scheduleFit();
}
function scheduleFit() {
  clearTimeout(fitTimer);
  fitTimer=setTimeout(()=>{ fitTimer=null; fitTerm(); },80);
}

const splitterBar=splitter();
if (splitterBar) {
  splitterBar.addEventListener('pointerdown',startDrag);
  splitterBar.addEventListener('pointermove',moveDrag);
  splitterBar.addEventListener('pointerup',endDrag);
  splitterBar.addEventListener('pointercancel',endDrag);
  splitterBar.addEventListener('dblclick',toggleDockMax);
  splitterBar.addEventListener('keydown',nudgeDock);
}

/* ---------------- project graph: derived numbers with their evidence --------
 * Every value comes from `graph_get`, which derives it from the signals ledger.
 * Unknown is an em dash, never 0 or 100, and verified and confidence sit beside
 * the headline because a percentage alone cannot be judged (BUG-084/089). */

/* -- progress: the headline, its parts, and the evidence behind them -------
 * One payload (`graph_get`) feeds three surfaces: the HUD in the strip, the ring
 * around the emblem, and this panel. It is loaded for the active project whether
 * or not the panel is open, so the main screen always shows where the work is. */
let graphData=null, graphLoading=false, graphSeq=0, graphDirty=false, graphFocus=null;
let graphRequested;              // the project the last load asked for
let graphEditing=false;          // the headline's set-% form is open
const expandedParts=new Set();   // part ids whose tasks are shown
const overview=new Map();        // project_id -> {percent, basis, confidence, verified, commits_since}
const scans=new Map();           // project_id -> the last scan digest this window saw
let assessing=null;              // {project, until, via} while an assessment runs
let graphTimer=null, overviewTimer=null, assessTimer=null;
const ASSESS_WINDOW=4*60*1000;

function graphNode(tag,className,text) {
  const node=document.createElement(tag);
  if (className) node.className=className;
  if (text!==undefined) node.textContent=String(text);
  return node;
}
function formatPercent(value) {
  if (value===null||value===undefined) return '—';
  const number=Number(value);
  if (!Number.isFinite(number)) return '—';
  return (Number.isInteger(number)?String(number):number.toFixed(1))+'%';
}
/* Headline numbers read as whole percents; the evidence keeps the decimals. */
function wholePercent(value) {
  if (value===null||value===undefined||value==='') return '—';
  const number=Number(value);
  return Number.isFinite(number)?Math.round(number)+'%':'—';
}
function formatConfidence(value) {
  if (value===null||value===undefined) return '—';
  const number=Number(value);
  if (!Number.isFinite(number)) return '—';
  return Math.round(Math.max(0,Math.min(1,number))*100)+'%';
}
function formatStamp(value) {
  const number=Number(value);
  if (!Number.isFinite(number)||number<=0) return '—';
  return new Date(number).toLocaleString(undefined,{month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'});
}
const SOURCE_NAMES={claude:'Claude Code',codex:'Codex',pi:'Pi',opencode:'OpenCode',agy:'Antigravity',
  navi:'Navi',detector:'the detector',user:'you',assistant:'the assistant'};
function sourceName(source) {
  const key=String(source||'');
  return SOURCE_NAMES[key]||key||'unknown';
}
function capitalise(text) {
  const value=String(text||'');
  return value?value.charAt(0).toUpperCase()+value.slice(1):value;
}
/* Unknown is not zero: a project with no estimate and no evidence shows a dash
 * (BUG-084/089). An estimate alone is a value, with or without tasks. */
function graphUnknown(graph) {
  return !graph||graph.percent_null===true||!Number.isFinite(Number(graph.percent));
}
function graphScoreLabel(graph) {
  return graphUnknown(graph)?'—':wholePercent(graph.percent);
}
/* What the headline stands on, in words: whose estimate and how old it is. */
function commitsSince(graph) {
  const commits=Number(graph?.commits_since)||0;
  return commits>0?`${commits} commit${commits===1?'':'s'} since`:'';
}
/* The HUD shows commitsSince on its label line instead; `full` adds it here. */
function basisText(graph,full) {
  const basis=graph?.basis||{};
  const since=full&&commitsSince(graph)?' · '+commitsSince(graph):'';
  if (basis.kind==='estimate') {
    return `Estimate by ${sourceName(basis.source)} · ${relativeTime(basis.time)}`+since;
  }
  if (basis.kind==='parts') {
    const parts=Number(basis.parts)||0;
    return `Across ${parts} part${parts===1?'':'s'}`+(basis.time?` · ${relativeTime(basis.time)}`:'')+since;
  }
  return 'No estimate yet';
}
function partBasis(aspect) {
  if (aspect.basis==='estimate') {
    const estimate=aspect.estimate||{};
    return `Estimate by ${sourceName(estimate.source)} · ${relativeTime(estimate.time||aspect.time)}`;
  }
  if (aspect.basis==='tasks') {
    const tasks=Number(aspect.tasks)||0;
    return `From ${tasks} task${tasks===1?'':'s'}`;
  }
  return 'No evidence yet';
}
function graphFor(project) {
  return graphData&&project&&graphData.project?.id===project.id?graphData:null;
}

function isAssessing(project) {
  if (assessing&&Date.now()>=assessing.until) assessing=null;
  return !!assessing&&!!project&&assessing.project===project;
}
function beginAssessment(project,via) {
  if (!project) return;
  assessing={project,until:Date.now()+ASSESS_WINDOW,via:String(via||'')};
  clearTimeout(assessTimer);
  assessTimer=setTimeout(()=>endAssessment(project),ASSESS_WINDOW+200);
  renderHud();
  renderGraph();
}
function endAssessment(project) {
  if (!assessing||(project&&assessing.project!==project)) return;
  assessing=null;
  clearTimeout(assessTimer);
  renderHud();
  renderGraph();
}

/* The panel is re-fetched for the project now active; a late reply from a
 * previous project is dropped by the ticket instead of overwriting this one. */
async function loadGraph() {
  const project=activeProjectId();
  const ticket=++graphSeq;
  graphRequested=project||null;
  if (!project) {
    graphData=null;
    graphLoading=false;
    renderGraph();
    renderHud();
    return null;
  }
  graphLoading=true;
  try {
    const result=await rpc('graph_get',{project_id:project});
    if (ticket!==graphSeq) return null;
    graphData=result&&result.project?result:null;
    if (graphData) {
      overview.set(project,{project_id:project,percent:graphData.percent,basis:graphData.basis?.kind,
        confidence:graphData.confidence,verified:graphData.verified,commits_since:graphData.commits_since});
    }
    return graphData;
  } catch (error) {
    if (ticket===graphSeq) {
      graphData=null;
      record('warn','graph unavailable: '+error.message);
    }
    return null;
  } finally {
    if (ticket===graphSeq) {
      graphLoading=false;
      renderGraph();
      renderHud();
      renderProjectSwitcher(true);
    }
  }
}
/* Progress events arrive in bursts (a harness reports each part, then the
 * whole), so the reload is debounced rather than run once per event. */
function scheduleGraph(delay=400) {
  clearTimeout(graphTimer);
  graphTimer=setTimeout(()=>loadGraph(),delay);
}
async function loadOverview() {
  try {
    const result=await rpc('progress_overview');
    overview.clear();
    for (const row of Array.isArray(result?.projects)?result.projects:[]) {
      if (row&&row.project_id) overview.set(String(row.project_id),row);
    }
    renderProjectSwitcher(true);
  } catch (error) {
    record('warn','progress overview unavailable: '+error.message);
  }
}
function scheduleOverview(delay=1500) {
  clearTimeout(overviewTimer);
  overviewTimer=setTimeout(loadOverview,delay);
}

/* Assess: scan the files, then ask a linked harness in this project (or Navi's
 * own assistant) to estimate each part. The answer arrives as progress events. */
async function assessProject() {
  const project=activeProjectId();
  if (!project) { openProjectDialog(); return null; }
  if (isAssessing(project)) return null;
  assessing={project,until:Date.now()+ASSESS_WINDOW,via:null};
  renderHud();
  renderGraph();
  try {
    const result=await rpc('project_detect',{project_id:project},30000);
    if (result?.scan?.digest) scans.set(project,{digest:String(result.scan.digest),time:Date.now()});
    if (result?.started===true) beginAssessment(project,result.via);
    else assessing=null;
    toast(String(result?.message||'Assessment started'));
    scheduleGraph(150);
    return result;
  } catch (error) {
    assessing=null;
    toast(error.message);
    return null;
  } finally {
    renderHud();
    renderGraph();
  }
}
async function reportProgress(params) {
  const project=activeProjectId();
  if (!project) throw new Error('Add a project first');
  return rpc('progress_report',{project_id:project,source:'user',confidence:1,create:false,
    evidence:'set by you in the progress panel',...params});
}

/* -- the HUD and the ring ---------------------------------------------------- */

function renderHud() {
  const hud=element('progress-hud');
  if (!hud) return;
  const project=activeProjectRow();
  hud.hidden=!project;
  if (!project) { window.naviScene?.setProgress?.(null,false); return; }
  const graph=graphFor(project);
  const unknown=graphUnknown(graph);
  const value=unknown?null:Number(graph.percent);
  const scanning=isAssessing(project.id);
  const percent=element('hud-percent');
  percent.textContent=unknown?'—':wholePercent(value);
  percent.dataset.null=String(unknown);
  element('hud-fill').style.width=unknown?'0%':`${Math.max(0,Math.min(100,value))}%`;
  element('hud-basis').textContent=scanning
    ?'Assessing…'
    :graph?basisText(graph):(graphLoading?'Reading…':'No estimate yet');
  hud.dataset.state=scanning?'assessing':unknown?'unknown':'known';
  const stale=element('hud-stale');
  stale.textContent=scanning?'':commitsSince(graph);
  stale.hidden=!stale.textContent;
  stale.title=stale.textContent?'Commits made after this estimate. Reassess to count them.':'';
  const assess=element('hud-assess');
  assess.disabled=scanning;
  assess.textContent=scanning?'Assessing…':unknown?'Assess':'Reassess';
  const open=element('hud-open');
  open.setAttribute('aria-expanded',String(graphOpen));
  open.title=graph
    ?`${project.name}: ${unknown?'no estimate yet':wholePercent(value)+' complete'}\n${String(graph.why||'')}\nClick for parts and evidence`
    :`${project.name}: click for parts and evidence`;
  window.naviScene?.setProgress?.(value,scanning);
}

/* -- the panel --------------------------------------------------------------- */

function graphStat(label,value) {
  const pair=graphNode('span','graph-stat');
  pair.append(graphNode('span','graph-stat-label',label),graphNode('strong','graph-stat-value',value));
  return pair;
}
function graphNotice(text) {
  return graphNode('p','graph-note',text);
}
/* A bar is text's companion, never its replacement: the value is always also
 * written beside it. `marker` places a tick where the tasks alone would put it. */
function meter(value,barClass,fillClass,marker) {
  const bar=graphNode('div',barClass);
  const known=value!==null&&value!==undefined&&Number.isFinite(Number(value));
  const fill=graphNode('i',fillClass);
  fill.dataset.state=known?'known':'unknown';
  fill.style.width=known?`${Math.max(0,Math.min(100,Number(value)))}%`:'0%';
  bar.append(fill);
  if (marker!==null&&marker!==undefined&&Number.isFinite(Number(marker))) {
    const tick=graphNode('i','meter-marker');
    tick.style.left=`${Math.max(0,Math.min(100,Number(marker)))}%`;
    tick.title=`Tasks alone: ${wholePercent(marker)}`;
    bar.append(tick);
  }
  return bar;
}
/* The whole-project estimates over time, oldest first, as one line. */
function trendLine(trend) {
  const points=(Array.isArray(trend)?trend:[])
    .map(point=>Number(point?.percent))
    .filter(Number.isFinite);
  if (points.length<2) return null;
  /* The axis spans the data (at least 10 points), so a 61 → 66 move reads as a
   * rise instead of a flat line on a 0–100 scale. */
  const low=Math.max(0,Math.min(...points)-3), high=Math.min(100,Math.max(...points)+3);
  const span=Math.max(10,high-low), floor=Math.max(0,Math.min(low,100-span));
  const width=96, height=26;
  const step=width/(points.length-1);
  const coords=points.map((value,index)=>
    `${(index*step).toFixed(1)},${(height-2-((value-floor)/span)*(height-4)).toFixed(1)}`);
  const svg=document.createElementNS(SVG_NS,'svg');
  svg.setAttribute('viewBox',`0 0 ${width} ${height}`);
  svg.setAttribute('width',String(width));
  svg.setAttribute('height',String(height));
  svg.setAttribute('class','graph-trend');
  svg.setAttribute('role','img');
  svg.setAttribute('aria-label',`Trend: ${points.map(value=>Math.round(value)+'%').join(', ')}`);
  const line=document.createElementNS(SVG_NS,'polyline');
  line.setAttribute('points',coords.join(' '));
  const end=document.createElementNS(SVG_NS,'circle');
  const [x,y]=coords[coords.length-1].split(',');
  end.setAttribute('cx',x); end.setAttribute('cy',y); end.setAttribute('r','2.2');
  svg.append(line,end);
  return svg;
}
function percentForm(options) {
  const form=graphNode('form','percent-form');
  form.autocomplete='off';
  const input=document.createElement('input');
  input.type='number'; input.min='0'; input.max='100'; input.step='1';
  input.placeholder='0–100';
  input.dataset.focus=options.focus;
  input.setAttribute('aria-label',options.label);
  if (Number.isFinite(Number(options.value))&&options.value!==null) input.value=String(Math.round(Number(options.value)));
  const unit=graphNode('span','percent-unit','%');
  const submit=graphNode('button','primary',options.action||'Set');
  submit.type='submit';
  form.append(input,unit,submit);
  form.addEventListener('submit',async event=>{
    event.preventDefault();
    const raw=input.value.trim();
    const value=Number(raw);
    if (!raw||!Number.isFinite(value)||value<0||value>100) {
      toast('Enter a percent from 0 to 100');
      input.focus();
      return;
    }
    submit.disabled=true;
    try {
      await options.submit(value);
      input.blur();
      await loadGraph();
    } catch (error) {
      toast(error.message);
      submit.disabled=false;
    }
  });
  form.addEventListener('keydown',event=>{
    if (event.key!=='Escape') return;
    event.preventDefault();
    event.stopPropagation();
    input.blur();
    options.cancel?.();
  });
  return form;
}
function textForm(options) {
  const form=graphNode('form','aspect-form');
  form.autocomplete='off';
  const input=document.createElement('input');
  input.type='text';
  input.maxLength=options.max||160;
  input.placeholder=options.placeholder;
  input.dataset.focus=options.focus;
  input.setAttribute('aria-label',options.label);
  if (options.id) input.id=options.id;
  const submit=graphNode('button','',options.action||'Add');
  submit.type='submit';
  form.append(input,submit);
  form.addEventListener('submit',async event=>{
    event.preventDefault();
    const title=input.value.trim();
    if (!title) { input.focus(); return; }
    submit.disabled=true;
    try {
      await options.submit(title);
      input.value='';
      graphFocus=options.focus;   // keep typing: the next one goes in the same field
      input.blur();
      await loadGraph();
    } catch (error) {
      toast(error.message);
      submit.disabled=false;
    }
  });
  return form;
}

function graphHeader(graph,project) {
  const head=graphNode('header','graph-head');
  const top=graphNode('div','graph-top');
  top.append(graphNode('span','graph-caption','Progress'));
  const name=graphNode('span','graph-project',project?[project.name,pathNote(project)].filter(Boolean).join(' · '):'No project');
  name.title=project?String(project.path||''):'';
  top.append(name);
  const close=graphNode('button','icon graph-close','×');
  close.type='button';
  close.setAttribute('aria-label','Close progress');
  close.addEventListener('click',()=>showGraph(false));
  top.append(close);
  head.append(top);
  const unknown=graphUnknown(graph);
  const score=graphNode('div','graph-score');
  const value=graphNode('strong','graph-percent',graphScoreLabel(graph));
  value.dataset.null=String(unknown);
  score.append(value,graphNode('span','graph-caption','complete'));
  const trend=trendLine(graph?.trend);
  if (trend) score.append(trend);
  head.append(score);
  head.append(meter(unknown?null:graph.percent,'graph-bar','meter-fill'));
  const scanning=isAssessing(project?.id);
  head.append(graphNode('p','graph-basis',scanning?'Assessing… estimates appear as they are reported.':basisText(graph,true)));
  if (graph?.why&&!scanning) head.append(graphNode('p','graph-why',String(graph.why)));
  /* With no estimate and no evidence there is no weight at all: confidence is
   * vacuous, so it reads as a dash rather than 0% (BUG-089). Verified measures
   * task weight backed by a test, commit or user verdict, so it sits with the
   * task count and is left out when there are no tasks: beside a model's
   * estimate, "Verified 0%" or "100%" would read as a verdict on that number. */
  const stats=graphNode('div','graph-sub');
  stats.append(graphStat('Confidence',unknown?'—':formatConfidence(graph.confidence)));
  const counts=graph?.tasks;
  if (counts&&Number(counts.total)>0) {
    stats.append(graphStat('Tasks',`${Number(counts.done)||0}/${Number(counts.total)}`));
    const verified=graphStat('Verified',formatPercent(graph.verified));
    verified.title='Share of task weight backed by a passing test, a commit or your verdict';
    stats.append(verified);
  }
  head.append(stats);
  const actions=graphNode('div','graph-actions');
  const assess=graphNode('button','primary',scanning?'Assessing…':unknown?'Assess':'Reassess');
  assess.type='button';
  assess.disabled=!project||scanning;
  assess.title='Scan the project, then ask a linked harness (or Navi) to estimate each part';
  assess.addEventListener('click',()=>assessProject());
  const set=graphNode('button','','Set %');
  set.type='button';
  set.disabled=!project;
  set.title='Set the whole-project percent yourself';
  set.setAttribute('aria-expanded',String(graphEditing));
  set.addEventListener('click',()=>{
    graphEditing=!graphEditing;
    if (graphEditing) graphFocus='overall';
    renderGraph();
  });
  const refresh=graphNode('button','','Refresh');
  refresh.type='button';
  refresh.addEventListener('click',()=>loadGraph());
  actions.append(assess,set,refresh);
  head.append(actions);
  if (graphEditing&&project) {
    head.append(percentForm({
      focus:'overall', label:'Whole-project percent', value:graph?.percent,
      submit:async value=>{ await reportProgress({percent:value}); graphEditing=false; },
      cancel:()=>{ graphEditing=false; renderGraph(); },
    }));
  }
  return head;
}

function taskList(items) {
  const list=graphNode('ul','task-list');
  for (const item of items) {
    const row=graphNode('li','task');
    const state=String(item.state||'todo');
    row.dataset.state=state;
    const box=document.createElement('input');
    box.type='checkbox';
    box.checked=state==='done';
    box.setAttribute('aria-label',`${String(item.title||'Task')}: ${box.checked?'done':'not done'}`);
    box.addEventListener('change',async ()=>{
      box.disabled=true;
      try {
        await rpc('task_set',{id:item.id,state:box.checked?'done':'todo'});
        await loadGraph();
      } catch (error) {
        toast(error.message);
        box.checked=!box.checked;
        box.disabled=false;
      }
    });
    const title=graphNode('span','task-title',String(item.title||'Task'));
    const known=item.percent!==null&&item.percent!==undefined;
    /* An open task at 0% is just open: the checkbox already says so. */
    const quiet=state==='todo'&&(!known||Number(item.percent)===0);
    const value=graphNode('span','task-value',quiet?'':known?wholePercent(item.percent):'—');
    value.title=known
      ?`${capitalise(sourceName(item.source))}${item.verified?' · verified':''}${item.time?' · '+relativeTime(item.time):''}`
      :'No evidence yet';
    row.append(box,title);
    if (state==='in_progress'||state==='blocked') {
      row.append(graphNode('span','task-flag',state==='blocked'?'blocked':'in progress'));
    }
    row.append(value);
    list.append(row);
  }
  return list;
}

function aspectRow(aspect) {
  const id=String(aspect.id||'');
  const title=String(aspect.title||aspect.key||'Part');
  const row=graphNode('li','aspect');
  row.dataset.aspect=id;
  row.dataset.basis=String(aspect.basis||'none');
  const open=expandedParts.has(id);
  const empty=aspect.percent_null===true||!Number.isFinite(Number(aspect.percent));
  const head=graphNode('button','aspect-head');
  head.type='button';
  head.setAttribute('aria-expanded',String(open));
  head.title=open?'Hide tasks':'Show tasks and set this part';
  head.dataset.focus='head:'+id;
  head.append(graphNode('span','aspect-caret',open?'▾':'▸'));
  head.append(graphNode('span','aspect-title',title));
  const value=graphNode('span','aspect-value',empty?'—':wholePercent(aspect.percent));
  value.dataset.null=String(empty);
  head.append(value);
  head.addEventListener('click',()=>{
    if (expandedParts.has(id)) expandedParts.delete(id); else expandedParts.add(id);
    graphFocus='head:'+id;   // the row is rebuilt; keyboard focus stays on it
    renderGraph();
  });
  row.append(head);
  /* A task mean of 0 usually means the tasks list remaining work (an
   * assessment adds them as todo), not that nothing is done: no marker then. */
  const derived=aspect.basis==='estimate'&&Number(aspect.derived)>0?aspect.derived:null;
  const bar=meter(empty?null:aspect.percent,'aspect-bar','aspect-fill',derived);
  bar.setAttribute('role','img');
  bar.setAttribute('aria-label',`${title}: ${empty?'unknown':wholePercent(aspect.percent)}`);
  row.append(bar);
  const meta=graphNode('div','aspect-meta');
  meta.append(graphNode('span','aspect-basis',partBasis(aspect)));
  const items=Array.isArray(aspect.items)?aspect.items:[];
  const tasks=Number(aspect.tasks)||items.length;
  if (tasks) {
    const done=items.filter(item=>item.state==='done').length;
    meta.append(graphNode('span','aspect-tasks',`${done}/${tasks} task${tasks===1?'':'s'} done`));
  }
  if (Number(aspect.unknown)>0) meta.append(graphNode('span','aspect-unknown',`${aspect.unknown} without evidence`));
  if (Number(aspect.weight)!==1) meta.append(graphNode('span','aspect-weight',`weight ${aspect.weight}`));
  row.append(meta);
  if (open) {
    const body=graphNode('div','aspect-body');
    const evidence=aspect.estimate?.evidence;
    if (evidence) body.append(graphNode('p','aspect-evidence',String(evidence)));
    if (Number.isFinite(Number(derived))&&derived!==null) {
      body.append(graphNotice(`Its tasks alone say ${wholePercent(derived)}. Verified task evidence newer than the estimate takes over.`));
    }
    if (items.length) body.append(taskList(items));
    body.append(textForm({
      focus:'task:'+id, label:`Add a task to ${title}`, placeholder:'Add a task', max:200,
      submit:title=>rpc('task_add',{project_id:activeProjectId(),aspect_id:id,title}),
    }));
    body.append(percentForm({
      focus:'part:'+id, label:`${title} percent`, value:aspect.percent, action:'Set part',
      submit:value=>reportProgress({aspect_id:id,percent:value}),
    }));
    row.append(body);
  }
  return row;
}

function emptyGraphState() {
  const block=graphNode('section','graph-empty');
  block.append(graphNode('h4','empty-title','No parts yet'));
  block.append(graphNode('p','empty-hint',
    'Parts split the project into areas, each with its own percent. Assess the project and a linked harness proposes them from the code, or add one here.'));
  return block;
}

function evidenceDrawer(signals,total) {
  const drawer=graphNode('details','graph-evidence');
  const shown=Math.min(signals.length,20);
  const count=Math.max(Number(total)||0,signals.length);
  drawer.append(graphNode('summary','graph-evidence-head',
    shown<count?`Evidence (${shown} of ${count})`:`Evidence (${count})`));
  if (!signals.length) {
    drawer.append(graphNotice('No evidence yet: an estimate from a harness, a checked task, a commit or a test line is what moves a number.'));
    return drawer;
  }
  const aspects=new Map((Array.isArray(graphData?.aspects)?graphData.aspects:[])
    .map(aspect=>[String(aspect.id),String(aspect.title||aspect.key||'')]));
  const list=graphNode('ul','evidence-list');
  for (const signal of signals.slice(0,20)) {
    const row=graphNode('li','evidence');
    row.dataset.source=String(signal.source||'');
    row.dataset.verified=String(signal.verified===true);
    const head=graphNode('div','evidence-head');
    const kind=String(signal.kind||'signal');
    /* Git rows are facts about the repository: their value is a flag, not a percent. */
    const git=kind.startsWith('git_');
    head.append(graphNode('span','evidence-kind',EVIDENCE_KINDS[kind]||kind.replace(/_/g,' ')));
    head.append(graphNode('span','evidence-source',capitalise(sourceName(signal.source))));
    const scope=git?'':signal.aspect_id?aspects.get(String(signal.aspect_id)):signal.task_id?'':'whole project';
    if (scope) head.append(graphNode('span','evidence-scope',scope));
    if (!git&&signal.value!==null&&signal.value!==undefined&&Number.isFinite(Number(signal.value))) {
      head.append(graphNode('span','evidence-value',wholePercent(Number(signal.value)*100)));
    }
    if (signal.verified===true) head.append(graphNode('span','evidence-verified','verified'));
    head.append(graphNode('span','evidence-time',relativeTime(signal.time)));
    head.title=formatStamp(signal.time)+(Number.isFinite(Number(signal.id))?' · #'+signal.id:'');
    row.append(head);
    /* "HEAD <hash> <subject>" reads as "<hash> <subject>"; older rows hold all 40 characters. */
    row.append(graphNode('p','evidence-text',String(signal.evidence||'').replace(/^HEAD ([0-9a-f]{12})(?:[0-9a-f]{28})?\b/,'$1')));
    list.append(row);
  }
  drawer.append(list);
  return drawer;
}

const EVIDENCE_KINDS={git_commit:'commit',git_clean:'clean tree',git_dirty:'uncommitted changes',
  test_pass:'tests pass',test_fail:'tests fail'};
function renderGraph() {
  const region=element('graph-region');
  if (!region) return;
  /* A refresh replaces the panel. While a text field in it holds the caret the
   * refresh waits for blur, so a progress event never eats what is being typed. */
  const focused=document.activeElement;
  if (focused&&region.contains(focused)&&focused.matches('input[type=text],input[type=number]')&&focused.value) {
    graphDirty=true;
    return;
  }
  graphDirty=false;
  const project=activeProjectRow();
  const graph=graphFor(project);
  const panel=graphNode('div','graph-panel');
  panel.append(graphHeader(graph,project));
  if (!graph) {
    panel.append(graphNotice(graphLoading
      ?'Reading progress…'
      :project?'Progress is unavailable right now.':'No project selected. Register one in Settings; progress is kept per project.'));
    region.replaceChildren(panel);
    return;
  }
  const aspects=Array.isArray(graph.aspects)?graph.aspects:[];
  const parts=graphNode('section','graph-parts');
  parts.append(graphNode('h4','graph-sub-title',aspects.length?`Parts (${aspects.length})`:'Parts'));
  if (aspects.length) {
    const list=graphNode('ul','aspect-list');
    list.setAttribute('aria-label','Parts');
    for (const aspect of aspects) list.append(aspectRow(aspect));
    parts.append(list);
  } else {
    parts.append(emptyGraphState());
  }
  parts.append(textForm({
    id:'aspect-title', focus:'part:new', label:'New part', placeholder:'Add a part, e.g. Voice', max:120,
    action:'Add part',
    submit:title=>rpc('aspect_add',{project_id:activeProjectId(),title}),
  }));
  panel.append(parts);
  const unassigned=Array.isArray(graph.unassigned)?graph.unassigned:[];
  if (unassigned.length) {
    const block=graphNode('section','graph-unassigned');
    block.append(graphNode('h4','graph-sub-title',`Tasks without a part (${unassigned.length})`));
    block.append(taskList(unassigned));
    panel.append(block);
  }
  const risks=Array.isArray(graph.risks)?graph.risks:[];
  if (risks.length) {
    const block=graphNode('section','graph-risks');
    block.append(graphNode('h4','graph-sub-title','Risks'));
    const list=graphNode('ul','risk-list');
    for (const risk of risks) list.append(graphNode('li','risk',String(risk)));
    block.append(list);
    panel.append(block);
  }
  const scan=scans.get(String(project?.id||''));
  if (scan) {
    const block=graphNode('details','graph-evidence graph-scan');
    block.append(graphNode('summary','graph-evidence-head','Last scan · '+relativeTime(scan.time)));
    block.append(graphNotice(scan.digest));
    panel.append(block);
  }
  panel.append(evidenceDrawer(Array.isArray(graph.signals)?graph.signals:[],graph.signal_count));
  const foot=graphNode('footer','graph-foot');
  foot.append(graphNode('span','graph-stamp','as of '+formatStamp(graph.as_of)));
  panel.append(foot);
  region.replaceChildren(panel);
  if (graphFocus) {
    const target=[...region.querySelectorAll('[data-focus]')].find(node=>node.dataset.focus===graphFocus);
    graphFocus=null;
    target?.focus();
  }
}
element('graph-region')?.addEventListener('focusout',()=>{
  if (!graphDirty) return;
  setTimeout(()=>{ if (graphDirty) renderGraph(); },0);
});

/* The rendered panel, not the payload: a test asserts what the reader sees. */
function graphSnapshot() {
  const region=element('graph-region');
  const stat=label=>{
    for (const pair of region?.querySelectorAll('.graph-stat')||[]) {
      if (pair.querySelector('.graph-stat-label')?.textContent===label) {
        return pair.querySelector('.graph-stat-value')?.textContent||null;
      }
    }
    return null;
  };
  return {
    open:graphOpen,
    loading:graphLoading,
    project_id:graphData?.project?.id||null,
    percent:graphData?.percent??null,
    percent_null:graphData?.percent_null===true,
    unknown:graphUnknown(graphData),
    basis:graphData?.basis?.kind||null,
    label:region?.querySelector('.graph-percent')?.textContent||null,
    hud:element('hud-percent')?.textContent||null,
    ring:window.naviScene?.progress?.()??null,
    assessing:!!assessing,
    verified:stat('Verified'),
    confidence:stat('Confidence'),
    aspects:[...(region?.querySelectorAll('.aspect')||[])].map(row=>({
      title:row.querySelector('.aspect-title')?.textContent||'',
      value:row.querySelector('.aspect-value')?.textContent||'',
      basis:row.dataset.basis||'',
      width:row.querySelector('.aspect-fill')?.style.width||'',
      empty:row.querySelector('.aspect-value')?.dataset.null==='true',
    })),
    risks:[...(region?.querySelectorAll('.risk')||[])].map(row=>row.textContent||''),
    evidence:region?.querySelectorAll('.evidence').length||0,
    evidenceTotal:region?.querySelector('.graph-evidence:not(.graph-scan) .graph-evidence-head')?.textContent||null,
    empty:!!region?.querySelector('.graph-empty'),
    as_of:region?.querySelector('.graph-stamp')?.textContent||null,
  };
}

/* The graph has its own cell; an empty panel is honest, invented edges are not. */
function showGraph(open) {
  const next=open===undefined?!graphOpen:!!open;
  const region=element('graph-region');
  graphOpen=next;
  if (region) {
    region.hidden=!next;
    if (next) {
      renderGraph();
      loadGraph();
    }
  }
  document.body.dataset.graph=next?'open':'closed';
  element('graph-tab')?.setAttribute('aria-pressed',String(next));
  element('hud-open')?.setAttribute('aria-expanded',String(next));
  syncFrame();
  scheduleFit();
  return next;
}
element('graph-tab')?.addEventListener('click',()=>showGraph(graphOpen?false:true));
element('hud-open')?.addEventListener('click',()=>showGraph());
element('hud-assess')?.addEventListener('click',()=>assessProject());
/* Relative times ("3m ago") age while the window idles. */
setInterval(()=>{ renderHud(); if (graphOpen&&!graphDirty) renderGraph(); },60000);

/* The scene measures #stage-region itself; the station only reports that rect on
 * a layout change and owns the dim channel, which no observer can infer. Below
 * 1280 px the graph becomes a drawer over the stage's right side, so the region is
 * not all visible: the art is framed by the largest sliver the drawer leaves.
 * Framed by the full region it ran under the graph and was cut by its straight
 * edge (BUG-023). */
function visibleSliver(cell, drawer) {
  const x0=Math.max(cell.x,drawer.x), x1=Math.min(cell.x+cell.w,drawer.x+drawer.w);
  const y0=Math.max(cell.y,drawer.y), y1=Math.min(cell.y+cell.h,drawer.y+drawer.h);
  if (x1-x0<=1||y1-y0<=1) return cell;  // the drawer misses the cell
  let best=null;
  for (const part of [
    {x:cell.x,y:cell.y,w:x0-cell.x,h:cell.h},
    {x:x1,y:cell.y,w:cell.x+cell.w-x1,h:cell.h},
    {x:cell.x,y:cell.y,w:cell.w,h:y0-cell.y},
    {x:cell.x,y:y1,w:cell.w,h:cell.y+cell.h-y1},
  ]) if (part.w>1&&part.h>1&&(!best||part.w*part.h>best.w*best.h)) best=part;
  return best||cell;
}
function stageRect() {
  const region=element('stage-region');
  if (!region) return null;
  const box=region.getBoundingClientRect();
  if (box.width<=1||box.height<=1) return null;
  const cell={x:box.x,y:box.y,w:box.width,h:box.height};
  const drawer=graphOpen?element('graph-region'):null;
  if (!drawer||drawer.hidden) return cell;
  const open=drawer.getBoundingClientRect();
  if (open.width<=1||open.height<=1) return cell;
  return visibleSliver(cell,{x:open.x,y:open.y,w:open.width,h:open.height});
}
function panelsOpen() { return dockMode()!=='closed'||graphOpen; }
/* The station is the only writer of the stage cell: it knows which drawers cover
 * the region, and it is the side that can observe the region. Watching it here
 * (rather than in the scene) means an early measurement — a boot that runs before
 * the stylesheet has laid the grid out — is corrected the moment the real cell
 * exists instead of framing the art to the window (BUG-020). */
function syncFrame() {
  const api=window.naviScene;
  if (typeof api?.setFrame!=='function') return;
  api.setFrame(stageRect(),panelsOpen()?1:0);
}
const stageRegion=element('stage-region');
if (stageRegion&&typeof ResizeObserver==='function') new ResizeObserver(()=>syncFrame()).observe(stageRegion);

addEventListener('resize',()=>{ reconcileDock(); syncFrame(); scheduleFit(); fitStrip(sessionsBar()); });

/* The strip is a session list, not a table dump: live sessions first, dead ones
 * collapsed under Recent, duplicates disambiguated, profile *names* instead of ids,
 * state and last activity visible, and every chip reconciled in place by id so the
 * 700 ms poll never rebuilds the row under the pointer (BUG-043). */
const sessionsBar=()=>element('sessions');
const sessionNodes=new Map(), sessionLabels=new Map();
let recentGroup=null, elsewhereGroup=null, addChip=null;
let launchWrap=null, launchMenu=null, launchOpen=false;
const SESSION_WORDS={starting:'starting',running:'running',idle:'ready',busy:'working',
  needs_input:'needs input',interrupted:'interrupted',exited:'exited'};
/* A collapsed chip group: finished sessions ("Recent") and live sessions of
 * other projects ("Elsewhere"). The toggle says how many it holds and, for
 * Elsewhere, whether one of them waits for input. */
function sessionGroup(label,title) {
  const wrap=document.createElement('div');
  wrap.className='sessions-recent';
  const toggle=document.createElement('button');
  toggle.type='button';
  toggle.className='session-chip recent-toggle';
  toggle.title=title;
  const chips=document.createElement('div');
  chips.className='sessions-recent-chips';
  wrap.append(toggle,chips);
  const group={label,wrap,toggle,chips,open:false,list:[]};
  toggle.onclick=()=>{ group.open=!group.open; renderSessions(snapshot?.sessions||[]); };
  return group;
}
function fillGroup(group,ids,attention) {
  if (!ids.length) {
    group.wrap.hidden=true;
    group.chips.hidden=true;
    group.list=[];
    orderNodes(group.chips,[]);
    return null;
  }
  group.wrap.hidden=false;
  group.toggle.textContent=`${group.label} (${ids.length})`+(attention?' · '+attention:'');
  group.toggle.classList.toggle('attention',!!attention);
  group.toggle.setAttribute('aria-expanded',String(group.open));
  group.chips.hidden=!group.open;
  if (!sameList(group.list,ids)) {
    orderNodes(group.chips,ids.map(id=>sessionNodes.get(id)).filter(Boolean));
    group.list=ids.slice();
  }
  return group.wrap;
}

function profileLabel(id) {
  const profiles=snapshot?.settings?.profiles||[];
  const match=profiles.find(profile=>profile.id===id);
  return match?(match.name||match.id):(id||'harness');
}
function relativeTime(ms) {
  const value=Number(ms)||0;
  if (!value) return 'unknown';
  const delta=Date.now()-value;
  if (delta<60000) return 'just now';
  if (delta<3600000) return Math.round(delta/60000)+'m ago';
  if (delta<86400000) return Math.round(delta/3600000)+'h ago';
  return Math.round(delta/86400000)+'d ago';
}
function createSessionChip(session) {
  const node=document.createElement('div');
  node.className='session-chip';
  node.setAttribute('role','button');
  node.tabIndex=0;
  node.dataset.id=session.id;
  const dot=document.createElement('span'); dot.className='session-dot'; dot.setAttribute('aria-hidden','true');
  const name=document.createElement('span'); name.className='session-name';
  const state=document.createElement('span'); state.className='session-state';
  node.append(dot,name,state);
  return node;
}
function updateSessionChip(node,session,label) {
  const live=session.running===true;
  node.dataset.live=String(live);
  node.dataset.state=String(session.state||(live?'running':'exited'));
  node.classList.toggle('dead',!live);
  node.classList.toggle('attached',live&&session.id===termSession);
  node.classList.toggle('active',session.active===true);
  if (session.active===true) node.setAttribute('aria-current','true'); else node.removeAttribute('aria-current');
  node.querySelector('.session-name').textContent=label;
  const stateText=node.querySelector('.session-state');
  stateText.textContent=live?(SESSION_WORDS[session.state]||String(session.state||'running')):'exited';
  node.dataset.harness=String(session.harness||'');
  const reopen=live?'':session.native_id?'Click to reopen and continue its conversation':'Click to reopen';
  node.title=[profileLabel(session.profile),session.cwd||'','last active '+relativeTime(session.last_active),reopen].filter(Boolean).join(' · ');
  /* A finished chip reopens its session in place: same name, and the harness's
   * own conversation when one was recorded (session_restart). */
  const activate=()=>live?openTerminal(session):reopenSession(session,label);
  node.onclick=event=>{
    if (event.target.closest('.session-drop')) return;
    activate();
  };
  node.onkeydown=event=>{
    if (event.key!=='Enter'&&event.key!==' ') return;
    event.preventDefault();
    activate();
  };
  /* Dead rows can be pruned on request; a live row keeps its terminal. */
  const drop=node.querySelector('.session-drop');
  if (live) { if (drop) drop.remove(); return; }
  let button=drop;
  if (!button) {
    button=document.createElement('button');
    button.type='button';
    button.className='session-drop';
    button.textContent='×';
    button.onclick=event=>{
      event.preventDefault();
      event.stopPropagation();
      removeSession(session);
    };
    node.append(button);
  }
  button.title='Remove this finished session';
  button.setAttribute('aria-label','Remove session '+label);
}
const reopening=new Set();
function reopenSession(session,label) {
  if (reopening.has(session.id)) return;
  reopening.add(session.id);
  toast('Reopening '+label+'…');
  rpc('session_restart',{id:session.id})
    .then(result=>{ if (result?.id) openTerminal(result); })
    .catch(error=>surfaceFailure(error.message))
    .finally(()=>reopening.delete(session.id));
}
function removeSession(session) {
  rpc('session_remove',{id:session.id})
    .then(()=>{ const node=sessionNodes.get(session.id); if (node) node.remove(); sessionNodes.delete(session.id); })
    .catch(error=>surfaceFailure(error.message));
}
/* Move only what is out of order and drop only what left the list: a chip that is
 * still in the strip is never detached and re-inserted, so a click being dispatched
 * cannot land on a detached node (the strip is reconciled in place, never rebuilt). */
function orderNodes(parent,desired) {
  let cursor=parent.firstChild;
  for (const node of desired) {
    if (!node) continue;
    if (node===cursor) { cursor=cursor.nextSibling; continue; }
    parent.insertBefore(node,cursor);
  }
  while (cursor) {
    const next=cursor.nextSibling;
    if (!desired.includes(cursor)) cursor.remove();
    cursor=next;
  }
}
function ensureSessionShell(bar) {
  if (!recentGroup) {
    elsewhereGroup=sessionGroup('Elsewhere','Live sessions in other projects');
    recentGroup=sessionGroup('Recent','Finished sessions');
  }
  if (!addChip) {
    /* One click opens a harness in the active project; the dialog stays behind
     * "More options" for another directory, a name or a first prompt. */
    launchWrap=document.createElement('div');
    launchWrap.className='launch-wrap';
    addChip=document.createElement('button');
    addChip.type='button';
    addChip.className='session-chip new';
    addChip.append('+',graphNode('span','new-label',' harness'));
    addChip.title='Open a harness in the active project';
    addChip.setAttribute('aria-label','Open a harness');
    addChip.setAttribute('aria-haspopup','menu');
    addChip.setAttribute('aria-expanded','false');
    addChip.onclick=event=>{
      event.stopPropagation();
      launchOpen=!launchOpen;
      renderLaunchMenu();
      if (launchOpen) {
        launchMenu.querySelector('.launch-option:not(:disabled)')?.focus();
        refreshHarnesses();
      }
    };
    launchMenu=document.createElement('div');
    launchMenu.className='launch-menu';
    launchMenu.setAttribute('role','menu');
    launchMenu.setAttribute('aria-label','Open a harness');
    launchMenu.hidden=true;
    launchWrap.append(addChip,launchMenu);
    document.addEventListener('click',event=>{
      if (!launchOpen||launchWrap.contains(event.target)) return;
      launchOpen=false;
      renderLaunchMenu();
    });
    launchWrap.addEventListener('keydown',event=>{
      if (!launchOpen) return;
      if (event.key==='Escape') {
        event.preventDefault();
        launchOpen=false;
        renderLaunchMenu();
        addChip.focus();
        return;
      }
      if (event.key!=='ArrowDown'&&event.key!=='ArrowUp') return;
      event.preventDefault();
      const items=[...launchMenu.querySelectorAll('button:not(:disabled)')];
      if (!items.length) return;
      const index=items.indexOf(document.activeElement);
      const step=event.key==='ArrowDown'?1:-1;
      items[(index+step+items.length)%items.length].focus();
    });
  }
  return bar;
}
function isTerminalProfile(profile) {
  return profile.id==='terminal'||profile.executable==='/bin/bash';
}
/* Which harness binaries exist on the launch PATH, from `harness_list` (the
 * daemon caches it for 60 s). Until the first answer every profile counts as
 * available: unknown availability must never disable a launch. A profile with
 * a full path counts as available unless the daemon reports that path missing. */
let harnessFound=null, harnessAsked=0, harnessMissingPaths=new Set();
async function refreshHarnesses() {
  if (Date.now()-harnessAsked<30000) return;
  harnessAsked=Date.now();
  try {
    const result=await rpc('harness_list')||{};
    const list=result.harnesses;
    if (!Array.isArray(list)) return;
    harnessFound=new Map(list.map(item=>[String(item.name),item.found===true]));
    harnessMissingPaths=new Set(Array.isArray(result.missing_paths)?result.missing_paths.map(String):[]);
    if (launchOpen) renderLaunchMenu();
    if (snapshot) surfaceSettings(snapshot);
    if (dialog.open) noteProfiles();
    if (newDialog.open) fillNewProfiles(element('new-profile').value);
  } catch { /* availability stays unknown */ }
}
function profileInstalled(profile) {
  if (!harnessFound||isTerminalProfile(profile)) return true;
  const executable=String(profile.executable||'');
  if (!executable) return true;
  if (executable.includes('/')) return !harnessMissingPaths.has(executable);
  const name=executable.toLowerCase();
  return harnessFound.has(name)?harnessFound.get(name):true;
}
function harnessProfiles() {
  const profiles=(snapshot?.settings?.profiles||[]).filter(profile=>profile&&profile.id);
  return [...profiles.filter(profile=>!isTerminalProfile(profile)),...profiles.filter(isTerminalProfile)];
}
function preferredProfile(profiles) {
  const list=(profiles||harnessProfiles()).filter(profile=>profile&&profile.id&&!isTerminalProfile(profile));
  return list.find(profileInstalled)||null;
}
function renderLaunchMenu() {
  if (!launchMenu) return;
  launchMenu.hidden=!launchOpen;
  addChip.setAttribute('aria-expanded',String(launchOpen));
  if (!launchOpen) { launchMenu.replaceChildren(); return; }
  const project=activeProjectRow();
  const items=[graphNode('p','launch-head',project?`Open in ${project.name}`:'Add a project first')];
  if (!project) {
    const add=graphNode('button','launch-option launch-more','Add project…');
    add.type='button';
    add.setAttribute('role','menuitem');
    add.onclick=event=>{
      event.stopPropagation();
      launchOpen=false;
      renderLaunchMenu();
      openProjectDialog();
    };
    items.push(add);
  }
  for (const profile of harnessProfiles()) {
    const button=graphNode('button','launch-option');
    button.type='button';
    button.setAttribute('role','menuitem');
    button.dataset.profile=String(profile.id);
    const binary=isTerminalProfile(profile)?'shell':String(profile.executable||'').split('/').pop();
    const installed=profileInstalled(profile);
    button.append(graphNode('span','launch-name',profile.name||profile.id));
    /* The binary is shown only when the name does not already say it. */
    if (!installed) {
      button.append(graphNode('span','launch-hint','not installed'));
      button.title=`${binary} is not on the launch PATH; install it or set its path in Settings`;
    } else if (binary&&binary.toLowerCase()!==String(profile.name||profile.id).toLowerCase()) {
      button.append(graphNode('span','launch-hint',binary));
    }
    button.disabled=!project?.path||!installed;
    button.onclick=event=>{
      event.stopPropagation();
      launchOpen=false;
      renderLaunchMenu();
      quickLaunch(profile);
    };
    items.push(button);
  }
  const more=graphNode('button','launch-option launch-more','More options…');
  more.type='button';
  more.setAttribute('role','menuitem');
  more.onclick=event=>{
    event.stopPropagation();
    launchOpen=false;
    renderLaunchMenu();
    openNewDialog();
  };
  items.push(more);
  launchMenu.replaceChildren(...items);
}
async function quickLaunch(profile) {
  const project=activeProjectRow();
  if (!project?.path) { openNewDialog(); return null; }
  try {
    const session=await rpc('create',{profile:profile.id,cwd:project.path});
    if (session?.ok===false&&session.error) toast(String(session.error));
    if (session?.id) openTerminal(session);
    return session;
  } catch (error) {
    toast(error.message);
    return null;
  }
}
/* Last seen state per session, so a harness that starts waiting for input is
 * announced once, on the transition, and never on the first render. */
const sessionStates=new Map();
function noteWaiting(session,label) {
  const before=sessionStates.get(session.id);
  sessionStates.set(session.id,session.state);
  if (before===undefined||before==='needs_input'||session.state!=='needs_input') return;
  /* The pane on screen already shows its own prompt. */
  if (session.id===termSession&&!element('terminal-pane')?.hidden) return;
  toast(`${label} is waiting for your input`);
}
function renderSessions(list) {
  const bar=sessionsBar();
  if (!bar) return;
  const sessions=Array.isArray(list)?list:[];
  bar.hidden=false;
  ensureSessionShell(bar);
  /* The active project's sessions come first; another project's keep a chip
   * (one click away) but carry that project's name, and numbering ("#2") counts
   * only same-named sessions of the same project and the same section. */
  const activeId=activeProjectId();
  const projectNames=new Map(projectList().map(project=>[project.id,project.name]));
  const elsewhere=session=>!!activeId&&!!session.project_id&&session.project_id!==activeId;
  const live=sessions.filter(session=>session.running===true)
    .sort((a,b)=>Number(elsewhere(a))-Number(elsewhere(b))||(Number(a.created)||0)-(Number(b.created)||0));
  /* A finished row that was reopened lives on as its replacement. */
  const dead=sessions.filter(session=>session.running!==true&&!session.replaced_by)
    .sort((a,b)=>(Number(b.last_active)||0)-(Number(a.last_active)||0));
  const labels=new Map();
  for (const group of [live,dead]) {
    const key=session=>(session.project_id||'')+'\u0000'+session.name;
    const counts=new Map(), seen=new Map();
    for (const session of group) counts.set(key(session),(counts.get(key(session))||0)+1);
    for (const session of group) {
      const index=(seen.get(key(session))||0)+1;
      seen.set(key(session),index);
      labels.set(session.id,counts.get(key(session))>1?session.name+' #'+index:session.name);
    }
  }
  const liveIds=[], elsewhereIds=[], deadIds=[];
  for (const session of [...live,...dead]) {
    const label=labels.get(session.id)||session.name;
    const other=elsewhere(session)?projectNames.get(session.project_id):null;
    let node=sessionNodes.get(session.id);
    if (!node) { node=createSessionChip(session); sessionNodes.set(session.id,node); }
    updateSessionChip(node,session,other?other+' · '+label:label);
    node.classList.toggle('elsewhere',!!other);
    sessionLabels.set(session.id,label);
    if (session.id===termSession) syncTerminalTitle(session);
    if (session.running===true) noteWaiting(session,other?other+' · '+label:label);
    (session.running!==true?deadIds:other?elsewhereIds:liveIds).push(session.id);
  }
  for (const [id,node] of [...sessionNodes]) {
    if (sessions.some(session=>session.id===id)) continue;
    node.remove();
    sessionNodes.delete(id);
    sessionLabels.delete(id);
    sessionStates.delete(id);
  }
  /* The switcher is the strip's first control: `orderNodes` removes every child
   * it is not given, so it must be part of the desired list (BUG-090). */
  const desired=[element('project-switcher'),...liveIds.map(id=>sessionNodes.get(id))];
  const waiting=elsewhereIds.some(id=>sessions.find(session=>session.id===id)?.state==='needs_input');
  desired.push(fillGroup(elsewhereGroup,elsewhereIds,waiting?'needs input':''));
  desired.push(fillGroup(recentGroup,deadIds,''));
  desired.push(launchWrap);
  orderNodes(bar,desired.filter(Boolean));
  fitStrip(bar);
}
/* Both layouts are measured in one task, so the one that wraps is never painted. */
function fitStrip(bar) {
  if (!bar) return;
  bar.classList.remove('compact');
  const items=[...bar.children].filter(node=>!node.hidden&&node.offsetParent!==null);
  const top=items[0]?.offsetTop??0;
  if (items.some(node=>node.offsetTop>top+4)) bar.classList.add('compact');
}

/* New-session dialog: pick a harness profile, project, optional prompt. */
const newDialog=element('new-dialog');
/* The dialog offers what the launch menu offers: harnesses before the plain
 * terminal, a harness that is not installed shown but not selectable, and the
 * first installed harness preselected instead of whatever profile is listed first. */
function fillNewProfiles(current) {
  const select=element('new-profile');
  select.replaceChildren();
  for (const profile of harnessProfiles()) {
    const entry=document.createElement('option');
    entry.value=profile.id;
    const installed=profileInstalled(profile);
    entry.textContent=(profile.name||profile.id)+(isTerminalProfile(profile)?' (terminal)':installed?'':' — not installed');
    entry.disabled=!installed;
    select.append(entry);
  }
  const options=[...select.options];
  const keep=options.find(entry=>entry.value===current&&!entry.disabled);
  const preferred=preferredProfile();
  select.value=keep?.value||preferred?.id||options.find(entry=>!entry.disabled)?.value||'';
  const missing=harnessProfiles().filter(profile=>!profileInstalled(profile)).map(profile=>profile.name||profile.id);
  element('new-profile-note').textContent=missing.length?'Not installed: '+missing.join(', ')+'. Settings can point a harness at its full path.':'';
}
function openNewDialog() {
  fillNewProfiles(null);
  refreshHarnesses();
  /* The default directory is the active project, not the first one: the
   * switcher decides where a new session lands (BUG-090). */
  const cwd=element('new-cwd');
  const active=activeProjectRow();
  cwd.value=active?.path||projectList()[0]?.path||'';
  const datalist=element('new-projects'); datalist.replaceChildren();
  for (const project of projectList()) {
    const entry=document.createElement('option'); entry.value=project.path; entry.label=project.name;
    datalist.append(entry);
  }
  element('new-name').value=''; element('new-prompt').value='';
  newDialog.showModal();
}
element('new-cancel').onclick=()=>newDialog.close();
element('new-form').onsubmit=async event=>{
  event.preventDefault();
  const profile=element('new-profile').value, cwd=element('new-cwd').value.trim();
  if (!profile||!cwd) { toast('Pick a harness and a project directory'); return; }
  const params={profile,cwd};
  const name=element('new-name').value.trim(); if (name) params.name=name;
  const prompt=element('new-prompt').value.trim(); if (prompt) params.prompt=prompt;
  try {
    const session=await rpc('create',params);
    newDialog.close();
    /* A session launched in another project makes that project active, so the
     * four scopes never disagree about where the work is (BUG-090). */
    if (session&&session.project_id&&session.project_id!==activeProjectId()) {
      switchProject(String(session.project_id));
    }
    openTerminal(session);
  } catch (error) { toast(error.message); }
};

/* ---- settings: model combobox, agent, harnesses, speech ---- */
const dialog=element('settings-dialog');
function option(select, value, label) {
  const entry=document.createElement('option'); entry.value=value; entry.textContent=label; select.append(entry);
}

/* One option list feeds two surfaces: the settings dialog and the composer
 * switcher. The current reference is always seeded, so a catalog that lacks the
 * saved model can never turn the field into an empty string (BUG-044). */
const modelState={value:'',filter:'',index:-1,open:false};
const composerModel={open:false,index:-1};

function modelChoices() {
  const choices=[];
  for (const item of catalogs.models.list||[]) {
    if (!item||!item.id) continue;
    const provider=String(item.providerID||'other');
    choices.push({value:provider+'/'+item.id,provider,label:String(item.name||item.id)});
  }
  const current=modelRef();
  if (current&&!choices.some(choice=>choice.value===current)) {
    choices.unshift({value:current,provider:current.split('/')[0]||'current',label:shortModel(current)+' (saved)'});
  }
  return choices;
}
function filterChoices(choices,query) {
  const needle=String(query||'').trim().toLowerCase();
  if (!needle) return choices;
  return choices.filter(choice=>choice.value.toLowerCase().includes(needle)
    ||choice.label.toLowerCase().includes(needle)
    ||choice.provider.toLowerCase().includes(needle));
}
function renderModelOptions(list,prefix,choices,value,activeIndex,onPick) {
  list.replaceChildren();
  let provider=null;
  choices.forEach((choice,index)=>{
    if (choice.provider!==provider) {
      provider=choice.provider;
      const group=document.createElement('li');
      group.className='combobox-group';
      group.setAttribute('role','presentation');
      group.textContent=provider;
      list.append(group);
    }
    const item=document.createElement('li');
    item.className='combobox-option'+(index===activeIndex?' active':'');
    item.id=prefix+'-option-'+index;
    item.dataset.value=choice.value;
    item.setAttribute('role','option');
    if (choice.value===value) item.setAttribute('aria-selected','true');
    const name=document.createElement('span'); name.className='option-name'; name.textContent=choice.label;
    const ref=document.createElement('span'); ref.className='option-ref'; ref.textContent=choice.value;
    item.append(name,ref);
    /* mousedown, not click: the field keeps focus, so blur cannot close the list
     * between the press and the release. */
    item.onmousedown=event=>{ event.preventDefault(); onPick(choice.value); };
    list.append(item);
  });
  return choices;
}

const settingsModelList=()=>element('model-options');
function settingsModelRender() {
  const input=element('setting-model');
  const choices=filterChoices(modelChoices(),modelState.filter);
  renderModelOptions(settingsModelList(),'model-option',choices,modelState.value,modelState.index,chooseSettingsModel);
  settingsModelList().hidden=!modelState.open||!choices.length;
  input.setAttribute('aria-expanded',String(modelState.open&&choices.length>0));
  return choices;
}
function chooseSettingsModel(value) {
  if (!isModelRef(value)) { toast('A model must look like provider/id'); return; }
  modelState.value=value;
  modelState.open=false;
  modelState.index=-1;
  const input=element('setting-model');
  input.value=value;
  input.setAttribute('aria-expanded','false');
  settingsModelList().hidden=true;
}
element('setting-model').addEventListener('focus',()=>{
  modelState.open=true; modelState.filter=''; modelState.index=-1;
  settingsModelRender();
});
element('setting-model').addEventListener('input',()=>{
  modelState.open=true;
  modelState.filter=element('setting-model').value;
  modelState.index=-1;
  settingsModelRender();
});
element('setting-model').addEventListener('blur',()=>setTimeout(()=>{
  modelState.open=false;
  settingsModelList().hidden=true;
  element('setting-model').setAttribute('aria-expanded','false');
},150));
element('setting-model').addEventListener('keydown',event=>{
  const choices=filterChoices(modelChoices(),modelState.filter);
  if (!modelState.open||!choices.length) return;
  if (event.key==='ArrowDown') {
    event.preventDefault();
    modelState.index=(modelState.index+1)%choices.length;
    settingsModelRender();
  } else if (event.key==='ArrowUp') {
    event.preventDefault();
    modelState.index=(modelState.index-1+choices.length)%choices.length;
    settingsModelRender();
  } else if (event.key==='Enter') {
    event.preventDefault();   // pick, never submit the dialog from the field
    const choice=choices[modelState.index>=0?modelState.index:0];
    if (choice) chooseSettingsModel(choice.value);
  } else if (event.key==='Escape') {
    event.preventDefault();
    modelState.open=false;
    settingsModelList().hidden=true;
  }
});

/* Composer switcher: the same catalog, one keystroke away from the message box. */
function openModelPopup() {
  if (!snapshot) return;
  composerModel.open=true;
  composerModel.index=-1;
  element('model-pop').hidden=false;
  element('model-switcher').setAttribute('aria-expanded','true');
  element('model-search').value='';
  composerModelRender();
  element('model-search').focus();
  if (!catalogs.models.list) {
    catalog('models','oc_models').then(()=>composerModelRender()).catch(()=>{
      /* The seeded current model is still listed; the failure is not a dead end. */
    });
  }
}
function closeModelPopup() {
  composerModel.open=false;
  element('model-pop').hidden=true;
  element('model-switcher').setAttribute('aria-expanded','false');
}
function composerModelRender() {
  const choices=filterChoices(modelChoices(),element('model-search').value);
  renderModelOptions(element('model-list'),'composer-model-option',choices,modelRef(),composerModel.index,chooseModel);
  element('model-list').hidden=!choices.length;
  return choices;
}
async function chooseModel(value) {
  if (!isModelRef(value)) { toast('A model must look like provider/id'); return; }
  closeModelPopup();
  if (!snapshot) return;
  const previous=String(snapshot.settings?.assistant?.model||'');
  if (value===previous) return;
  const next={...snapshot.settings,assistant:{...(snapshot.settings.assistant||{}),model:value}};
  try {
    await rpc('settings',next);
    snapshot.settings=next;
    surfaceSettings(snapshot);
    toast('Model set to '+shortModel(value));
  } catch (error) { surfaceFailure('Could not change the model: '+error.message); }
}
element('model-switcher').onclick=()=>{ if (composerModel.open) closeModelPopup(); else openModelPopup(); };
element('model-search').addEventListener('input',()=>composerModelRender());
element('model-search').addEventListener('keydown',event=>{
  const choices=filterChoices(modelChoices(),element('model-search').value);
  if (!choices.length) return;
  if (event.key==='ArrowDown') {
    event.preventDefault();
    composerModel.index=(composerModel.index+1)%choices.length;
    composerModelRender();
  } else if (event.key==='ArrowUp') {
    event.preventDefault();
    composerModel.index=(composerModel.index-1+choices.length)%choices.length;
    composerModelRender();
  } else if (event.key==='Enter') {
    event.preventDefault();
    const choice=choices[composerModel.index>=0?composerModel.index:0];
    if (choice) chooseModel(choice.value);
  } else if (event.key==='Escape') {
    event.preventDefault();
    closeModelPopup();
    element('model-switcher').focus();
  }
});
document.addEventListener('mousedown',event=>{
  if (!composerModel.open) return;
  if (element('model-pop').contains(event.target)||element('model-switcher').contains(event.target)) return;
  closeModelPopup();
});

async function fillSettingsSelects(settings) {
  const agent=element('setting-agent');
  const agentId=settings.assistant?.agent||'navi';
  modelState.value=String(settings.assistant?.model||modelRef());
  element('setting-model').value=modelState.value;
  element('setting-model').setAttribute('aria-expanded','false');
  element('model-note').textContent='The answering model is reported with every reply.';
  try {
    await catalog('models','oc_models');
    /* A catalog that lands after the field was focused must reach the open list. */
    if (modelState.open) settingsModelRender();
  } catch (error) {
    element('model-note').textContent='Model catalog unavailable: '+error.message+' — the saved model is kept.';
  }
  /* The seeded agent is always selectable: a catalog that arrives later replaces the
   * list in one synchronous step, so a save can never read an empty field. */
  agent.replaceChildren();
  option(agent,agentId,agentId);
  agent.value=agentId;
  element('agent-note').textContent='';
  try {
    const agents=await catalog('agents','oc_agents');
    const known=agents.filter(item=>item&&item.id);
    if (known.length) {
      agent.replaceChildren();
      for (const item of known) option(agent,item.id,item.name||item.id);
      if (![...agent.options].some(entry=>entry.value===agentId)) option(agent,agentId,agentId);
    }
    agent.value=agentId;
  } catch (error) {
    agent.replaceChildren();
    option(agent,agentId,agentId);
    agent.value=agentId;
    element('agent-note').textContent='Agent list unavailable: '+error.message+' — the saved agent is kept.';
  }
}
/* BUG-077: arguments must round-trip spaces and quoting exactly; the old
 * whitespace join destroyed `--flag "two words"` on every save. The field shows
 * shell words (`--flag 'two words'`) and parses them back with the same quoting
 * rules, so the round trip is exact without asking the user to write JSON. */
function formatArgs(args) {
  return (args||[]).map(arg=>{
    const text=String(arg);
    if (/^[A-Za-z0-9_@%+=:,./-]+$/.test(text)) return text;
    return "'"+text.replace(/'/g,"'\\''")+"'";
  }).join(' ');
}
function parseArgs(text) {
  const raw=String(text||'').trim();
  if (raw.startsWith('[')) {
    const args=JSON.parse(raw);
    if (!Array.isArray(args)||args.some(arg=>typeof arg!=='string')) throw new Error('a JSON list must hold only strings');
    return args;
  }
  const args=[];
  let current='', started=false, quote=null;
  for (let index=0;index<raw.length;index++) {
    const char=raw[index];
    if (quote==="'") { if (char==="'") quote=null; else current+=char; continue; }
    if (quote==='"') {
      if (char==='"') quote=null;
      else if (char==='\\'&&index+1<raw.length&&'"\\$`'.includes(raw[index+1])) current+=raw[++index];
      else current+=char;
      continue;
    }
    if (/\s/.test(char)) { if (started) { args.push(current); current=''; started=false; } continue; }
    started=true;
    if (char==="'"||char==='"') quote=char;
    else if (char==='\\'&&index+1<raw.length) current+=raw[++index];
    else current+=char;
  }
  if (quote) throw new Error('a '+(quote==='"'?'double':'single')+' quote is not closed');
  if (started) args.push(current);
  return args;
}
function profileRow(profile={id:'',name:'',executable:'',args:[]}) {
  const row=document.createElement('div'); row.className='profile-row';
  /* The daemon's id is not derivable from the display name ("Claude Code" is
   * `claude`), so the row carries it: a save must not read as a rename. */
  row.dataset.id=profile.id||'';
  const label=profile.name||profile.id||'new harness';
  const make=(cls,placeholder,value,name)=>{
    const field=document.createElement('input'); field.className=cls; field.placeholder=placeholder; field.value=value;
    field.spellcheck=false; field.autocomplete='off';
    field.setAttribute('aria-label',name+' for '+label);
    row.append(field); return field;
  };
  /* Example text only on a fresh row: five identical placeholders read as values. */
  const fresh=!profile.id;
  make('profile-name',fresh?'Name':'',profile.name||'','Name');
  make('profile-exec',fresh?'codex or /path/to/bin':'',profile.executable||'','Command').oninput=noteProfiles;
  make('profile-args',fresh?'--flag value':'',formatArgs(profile.args),'Arguments');
  const drop=document.createElement('button'); drop.type='button'; drop.className='icon profile-remove'; drop.textContent='×';
  drop.title='Remove harness'; drop.setAttribute('aria-label','Remove '+label);
  drop.onclick=()=>{ row.remove(); noteProfiles(); };
  row.append(drop);
  return row;
}
/* Commands the launch PATH probe did not find are outlined and named once under
 * the list; an unknown probe result marks nothing. */
function noteProfiles() {
  const missing=[];
  for (const row of element('profiles-editor').children) {
    const field=row.querySelector('.profile-exec');
    const executable=field.value.trim();
    const absent=!!executable&&!profileInstalled({id:row.dataset.id,executable});
    field.classList.toggle('missing',absent);
    field.title=absent?executable+' is not on the launch PATH':'';
    if (absent) missing.push(executable);
  }
  element('profiles-note').textContent=missing.length
    ?'Not found on the launch PATH: '+[...new Set(missing)].join(', ')+'. Install it, or give its full path.'
    :'';
}
function collectProfiles() {
  /* The editor edits three fields and the daemon owns the rest (harness, env,
   * cwd, resume policy), so a known profile is rebuilt from the daemon's own
   * record: sending a stripped copy read as a removal and the save was refused. */
  const known=new Map((snapshot?.settings?.profiles||[]).map(profile=>[profile.id,profile]));
  const profiles=[];
  for (const row of element('profiles-editor').children) {
    const name=row.querySelector('.profile-name').value.trim();
    const exec=row.querySelector('.profile-exec').value.trim();
    const original=known.get(row.dataset.id);
    if (!exec&&!original) continue;  // a fresh row nobody filled in
    let args;
    try { args=parseArgs(row.querySelector('.profile-args').value); }
    catch (error) { throw new Error('Arguments for "'+(name||exec||'harness')+'": '+error.message); }
    profiles.push({
      ...original,
      id:original?original.id
        :(name||exec).toLowerCase().replace(/[^a-z0-9_-]+/g,'-').replace(/^-+|-+$/g,'')||exec,
      name:name||original?.name||exec,
      executable:exec||original?.executable||'',
      args,
    });
  }
  return profiles;
}
element('profile-add').onclick=()=>{
  const row=profileRow();
  element('profiles-editor').append(row);
  row.querySelector('.profile-name').focus();
};
/* Setup for harnesses the user starts outside Navi: fetched once per open,
 * since the snippets carry this daemon's binary path. */
let linkSetups=[], linkChoice='';
function showLinkSetup(harness) {
  const setup=linkSetups.find(item=>item.harness===harness)||linkSetups[0];
  if (!setup) return;
  linkChoice=setup.harness;
  for (const button of element('link-tabs').children) button.setAttribute('aria-pressed',String(button.dataset.harness===linkChoice));
  element('link-place').textContent=setup.place;
  element('link-snippet').textContent=setup.snippet;
  element('link-copy').disabled=false;
}
async function fillLinkSetup() {
  const tabs=element('link-tabs');
  element('link-copy').disabled=true;
  try {
    linkSetups=(await rpc('link_setup')).harnesses||[];
  } catch (error) {
    linkSetups=[];
    tabs.replaceChildren();
    element('link-snippet').textContent='';
    element('link-place').textContent='Could not read the setup: '+error.message;
    return;
  }
  tabs.replaceChildren(...linkSetups.map(setup=>{
    const button=document.createElement('button');
    button.type='button'; button.dataset.harness=setup.harness; button.textContent=setup.title;
    button.onclick=()=>showLinkSetup(setup.harness);
    return button;
  }));
  const preferred=preferredProfile();
  showLinkSetup(linkChoice||String(preferred?.executable||preferred?.id||'').split('/').pop());
}
element('link-outside').ontoggle=()=>{ if (element('link-outside').open) fillLinkSetup(); };
element('link-copy').onclick=async()=>{
  const text=element('link-snippet').textContent;
  toast(text&&await copyText(text)?'Setup copied':'Could not copy the setup');
};
function openSettings() {
  if (!snapshot) { toast('Navi is still connecting'); return; }
  const settings=snapshot.settings;
  element('setting-speak').checked=!!settings.speak;
  element('setting-mic').checked=!!settings.microphone;
  element('setting-prompt').value=settings.system_prompt||'';
  const language=String(settings.speech?.language||'en').trim()||'en';
  element('setting-language').value=language;
  element('speech-note').textContent='Speech language: '+language+' · the wake-word model is English-only.';
  const editor=element('profiles-editor');
  editor.replaceChildren();
  for (const profile of settings.profiles||[]) editor.append(profileRow(profile));
  noteProfiles();
  refreshHarnesses();
  if (element('link-outside').open) fillLinkSetup();
  dialog.showModal();
  fillSettingsSelects(settings);
  fillDiagnostics();
}
element('settings-button').onclick=()=>openSettings();
element('settings-cancel').onclick=()=>dialog.close();
element('settings-new').onclick=async()=>{
  try {
    await rpc('oc_new',{project_id:activeProjectId()||undefined});
    invalidateCatalogs();     // a new gateway session can expose a different catalog
    toast('Started a new OpenCode session');
  } catch (error) { toast(error.message); }
};
element('settings-form').onsubmit=async event=>{
  event.preventDefault();
  if(!snapshot)return;
  /* BUG-044: an empty or malformed reference breaks every later turn, so an
   * invalid field is refused instead of written over a working model. */
  const candidate=modelState.value||element('setting-model').value.trim()||modelRef();
  if (!isModelRef(candidate)) {
    element('model-note').textContent='Keep a model reference shaped like provider/model.';
    toast('Save refused: the assistant model must look like provider/model');
    return;
  }
  /* An empty field falls back to the stored value: a save must never write a broken
   * agent reference either (BUG-044 covers the model, the same rule holds here). */
  const agentValue=element('setting-agent').value||snapshot.settings.assistant?.agent||'navi';
  let profiles;
  try { profiles=collectProfiles(); }
  catch (error) { toast(error.message); return; }
  /* BUG-057: the transcript/TTS language is a setting, never a hardcoded "en".
   * BCP-47-ish codes only; the wake-word model itself stays English-only. */
  const language=String(element('setting-language').value||'en').trim().toLowerCase();
  if (!/^[a-z]{2,3}(-[a-z]{2})?$/.test(language)) {
    toast('Speech language must be a code like en, ar or pt-br');
    element('setting-language').focus();
    return;
  }
  const next={
    ...snapshot.settings,
    speak:element('setting-speak').checked,
    microphone:element('setting-mic').checked,
    speech:{...(snapshot.settings.speech||{}),language},
    system_prompt:element('setting-prompt').value,
    assistant:{
      ...(snapshot.settings.assistant||{}),
      model:candidate,
      agent:agentValue,
    },
    profiles,
  };
  try {
    await rpc('settings',next);
    snapshot.settings=next;
    modelState.value=candidate;
    try { await rpc('voice_start'); } catch (error) { /* workers stay off in isolated mode */ }
    dialog.close();
    surfaceSettings(snapshot);
    toast('Settings saved');
  } catch (error) { surfaceFailure(error.message); }
};

/* ---------------- diagnostics: visible state, no network ---------------- */

function diagnosticsBlock() {
  let block=element('diagnostics');
  if (block) return block;
  const form=element('settings-form');
  block=document.createElement('section');
  block.id='diagnostics';
  block.setAttribute('aria-label','Diagnostics');
  block.style.cssText='margin-top:12px;padding:10px;border:1px solid rgba(141,255,224,.22);border-radius:6px;background:rgba(3,10,43,.5)';
  const title=document.createElement('b');
  title.textContent='Diagnostics';
  const body=document.createElement('pre');
  body.id='diagnostics-body';
  body.style.cssText='margin:6px 0;white-space:pre-wrap;font:11px/1.45 ui-monospace,Menlo,Consolas,monospace';
  body.textContent='collecting…';
  const row=document.createElement('div');
  row.style.cssText='display:flex;gap:8px;flex-wrap:wrap';
  const reload=document.createElement('button');
  reload.type='button'; reload.id='diagnostics-reload'; reload.textContent='Reload UI';
  reload.onclick=()=>{ if(!send({type:'reload'})) location.reload(); toast('Reloading the interface'); };
  const copy=document.createElement('button');
  copy.type='button'; copy.id='diagnostics-copy'; copy.textContent='Copy diagnostics';
  copy.onclick=copyDiagnostics;
  row.append(reload,copy);
  block.append(title,body,row);
  const actions=form.querySelector('.actions');
  if (actions) form.insertBefore(block,actions); else form.append(block);
  return block;
}
function diagnosticsText() {
  const ratio=window.naviScene?.renderer?.getPixelRatio?.();
  const lines=[
    `renderer   ${element('field').dataset.renderer||'unknown'}${typeof ratio==='number'?' · pixel ratio '+ratio:''}`,
    `dpr        ${devicePixelRatio||1}`,
    `viewport   ${innerWidth}×${innerHeight}`,
    `daemon     ${daemon?'navid '+daemon.version+' pid '+daemon.pid:'not reachable'}`,
    `pending    ${pending.size} · ${timeouts} timeouts · ${dropped} dropped`,
    `cursor     ${seq} · boot ${snapshot?.boot_id ?? 'unknown'}`,
    `chat       ${messages.length} messages · ${mountedRows.size} mounted · ${statusPhase}${turn?' · turn open':''}`,
    `voice      ${voiceWord(snapshot?.voice?.status)}${confirmState.active?' · confirming':''}`,
    `workers    voice ${snapshot?.voice?.status||'unknown'} · desktop ${snapshot?.desktop?.status||'unknown'}`,
    `feed       ${offline?'offline':'online'} · ${polls} polls · ${pollFailures} failures · backoff ${backoff}ms`,
  ];
  const last=diagnostics.at(-1);
  if (last) lines.push(`last log   ${last.level}: ${last.message}`);
  return lines.join('\n');
}
async function fillDiagnostics() {
  diagnosticsBlock();
  if (!daemon) await refreshDaemon();
  const body=element('diagnostics-body');
  if (body) body.textContent=diagnosticsText();
}
async function copyDiagnostics() {
  const text=`Navi diagnostics\n${diagnosticsText()}\n\n${JSON.stringify(window.navi.inspect(),null,2)}`;
  toast(await copyText(text)?'Diagnostics copied':'Could not copy diagnostics');
}

/* Panels that carry content the user reads; the stage is a measurement cell and
 * is inspected separately, because a drawer is allowed to sit on it by design. */
const LAYOUT_PANELS=['sessions','chat','terminal','graph'];
function rectOf(target) {
  const node=typeof target==='string'?document.querySelector(target):target;
  if (!node) return null;
  const box=node.getBoundingClientRect(), style=getComputedStyle(node);
  return {x:Math.round(box.x),y:Math.round(box.y),width:Math.round(box.width),height:Math.round(box.height),
    visible:style.display!=='none'&&style.visibility!=='hidden'&&box.width>1&&box.height>1};
}
function intersects(a,b) {
  return Math.min(a.x+a.width,b.x+b.width)-Math.max(a.x,b.x)>1
    && Math.min(a.y+a.height,b.y+b.height)-Math.max(a.y,b.y)>1;
}
/* The boot overlay is a flex box in the stylesheet, so `hidden` alone would not
 * close it; the evidence asserts the computed display instead of the flag. */
function bootOverlay() {
  const node=element('boot');
  if (!node) return null;
  const style=getComputedStyle(node);
  return {display:style.display,hidden:node.hidden,opacity:style.opacity};
}
function layoutSnapshot() {
  const layout={
    stage:rectOf('#stage-region'),
    chat:rectOf('.chat'),
    terminal:rectOf('#terminal-pane'),
    sessions:rectOf('#sessions'),
    graph:rectOf('#graph-region'),
  };
  /* The graph becomes a drawer over the stage cell below 1280 px and the terminal
   * takes that cell below 900, so the stage is excluded from the pair list: two
   * content panels over each other is the defect this reports. */
  const overlaps=[];
  for (let a=0;a<LAYOUT_PANELS.length;a++) for (let b=a+1;b<LAYOUT_PANELS.length;b++) {
    const first=layout[LAYOUT_PANELS[a]], second=layout[LAYOUT_PANELS[b]];
    if (first?.visible&&second?.visible&&intersects(first,second)) overlaps.push(LAYOUT_PANELS[a]+'/'+LAYOUT_PANELS[b]);
  }
  return {...layout,overlaps};
}

window.navi={
  reply(ticket,reply){
    /* Isolated-only test hook: prove a lost reply becomes a timeout instead of
     * a permanently parked promise. */
    if (host.dropReply>0) { host.dropReply--; dropped++; record('warn',`dropped reply for ticket ${ticket}`); return; }
    const request=pending.get(ticket);
    if(!request) { unknownReplies++; record('warn',`reply for unknown ticket ${ticket}`); return; }
    pending.delete(ticket);
    if(reply?.ok) { request.resolve(reply.result); return; }
    const error=new Error((reply&&reply.error)||'Request failed');
    record('error',`rpc ${request.op} failed: ${error.message}`);
    request.reject(error);
  },
  receive(data){term?.write(Uint8Array.from(atob(data),c=>c.charCodeAt(0)));},
  notice:toast,
  /* BUG-015 push channel: the host delivers a `status` payload over the existing
   * message bridge (40 ms while a turn is open, 700 ms idle). The poll stays as the
   * fallback and both paths go through the same applier. */
  push(value){
    try { update(value); }
    catch (error) { record('warn','push failed: '+error.message); }
  },
  inspect(){
    const ratio=window.naviScene?.renderer?.getPixelRatio?.();
    const chat=document.querySelector('.chat');
    /* The computed display is the truth: the `hidden` attribute alone would miss
     * a panel the CSS closed (and vice versa). */
    const terminalDisplay=getComputedStyle(element('terminal-pane')).display;
    return {
      renderer:element('field').dataset.renderer||'unknown',
      pixelRatio:typeof ratio==='number'?ratio:null,
      dpr:devicePixelRatio||1,
      viewport:{width:innerWidth,height:innerHeight},
      chatOpacity:chat?getComputedStyle(chat).opacity:null,
      booted,degraded,boot:bootSource,
      bootOverlay:bootOverlay(),
      /* The painted art, not the layout: a layout probe can only see panel
       * rects, so the stage reports where its own drawing actually landed. */
      art:window.naviScene?.bounds?.() ?? null,
      devtools:host.devtools===true,isolated:host.isolated===true,
      pending:pending.size,
      pendingOps:[...pending.values()].map(entry=>entry.op),
      /* A healthy request can still be in flight when a snapshot is taken; a
       * request older than its delivery budget is the parked promise the sweep
       * exists to prevent. */
      pendingStale:[...pending.values()]
        .filter(entry=>Date.now()-(entry.expires-entry.budget)>entry.budget).length,
      pendingOldestMs:[...pending.values()]
        .reduce((oldest,entry)=>Math.max(oldest,Date.now()-(entry.expires-entry.budget)),0),
      cursor:seq,
      polls,pollFailures,offline,backoff,
      timeouts,dropped,unknownReplies,
      daemon:daemon||null,
      workers:{voice:snapshot?.voice?.status||'unknown',desktop:snapshot?.desktop?.status||'unknown'},
      errors:diagnostics.filter(entry=>entry.level==='error'),
      diagnostics:diagnostics.slice(),
      sessions:snapshot?.sessions,
      title:'Navi',
      messages:document.querySelectorAll('#chat-log .msg').length,
      chat:{
        count:messages.length,
        mounted:mountedRows.size,
        rows:document.querySelectorAll('#chat-log .msg').length,
        empty:!element('chat-empty').hidden,
        emptyVisible:!element('chat-empty').hidden,
        streaming:!!turn&&turn.done!==true,
        turnDone:!!turn&&turn.done===true,
        status:statusPhase,
        label:element('state').textContent,
        failed:messages.filter(message=>message.status==='failed').length,
        ids:messages.map(message=>message.id),
        cursors:messages.map(message=>message.cursor),
        savedIds:messages.filter(message=>message.role==='user'||message.role==='assistant').map(message=>message.id),
        lastId:messages.at(-1)?.id||null,
        lastText:messages.length?messageText(messages.at(-1)):'',
        lastStatus:messages.at(-1)?.status||null,
        pendingFinalise:pendingFinalise?pendingFinalise.id:null,
        history:{limit:historyLimit,hasMore:historyHasMore,oldestCursor},
        lastFailure:lastFailure||null,
        attachments:attachments.map(entry=>({name:entry.name,size:entry.size,
          ready:!!entry.data,error:entry.error||null})),
        lastCommand,
      },
      voice:{
        status:snapshot?.voice?.status||'unknown',
        label:voiceWord(snapshot?.voice?.status),
        transcript:snapshot?.voice?.transcript||null,
        confirming:confirmState.active,
        countdown:element('voice-count')?.textContent||'',
        banner:!element('voice-banner').hidden,
      },
      model:shortModel(modelRef()),
      modelPicker:{value:modelState.value,filter:modelState.filter,open:modelState.open,
        choices:modelChoices().length,catalog:(catalogs.models.list||[]).length,
        options:document.querySelectorAll('#model-options .combobox-option').length},
      rpcOps:rpcLog.slice(-40),
      rpcCalls:rpcCalls.slice(),
      slash:slashMatches.length,
      busy,
      terminal:terminalDisplay!=='none',
      terminalDisplay,
      dock:dockMode(),
      graphOpen,
      phase,
      /* The active project and the rendered graph, so a test asserts the same
       * frame the reader sees (BUG-089/090). */
      project:{
        id:activeProjectId(),
        name:activeProjectRow()?.name||null,
        path:activeProjectRow()?.path||null,
        path_state:activeProjectRow()?pathState(activeProjectRow()):null,
        label:element('project-status')?.textContent||'',
        switcher:switcherOpen,
        options:projectList().length,
        scope:assistantScope,
      },
      graph:graphSnapshot(),
      layout:layoutSnapshot(),
    };
  },
  /* Programmatic layout controls: the dock is also driven by the session chips,
   * and the graph panel by its own refresh and tab. */
  setDock:mode=>setDock(mode),
  showGraph:open=>showGraph(open),
  switchProject:id=>switchProject(id),
  loadGraph:()=>loadGraph(),
  graph:()=>graphSnapshot(),
};

/* Isolated-only hooks: the GUI workflow drives the real page, so it needs a way to
 * reset the transcript and re-hydrate it from the daemon's own store. */
if (host.isolated===true) {
  window.navi.hooks={
    update,
    push:value=>{ try { update(value); } catch (error) { record('warn','push failed: '+error.message); } },
    resetChat:()=>resetChat(),
    pausePoll:value=>{
      pollPaused=!!value;
      if (!pollPaused) poll();
    },
    hydrate:options=>loadHistory(options||{}),
    dispatch:(text,options)=>dispatch(text,options||{}),
    attach:(items)=>{
      let accepted=0;
      for (const item of items||[]) if (acceptAttachment(item)) accepted+=1;
      renderAttachments();
      return {accepted,count:attachments.length};
    },
    clearAttachments:()=>{ attachments=[]; renderAttachments(); },
    lastCommand:()=>lastCommand,
    ids:()=>messages.map(message=>message.id),
    rpcOps:()=>rpcLog.slice(),
  };
}

/* ---------------- boot, degradation, and stage DPR ---------------- */

/* The station owns booting: the scene callback, the first uncaught error, and a
 * deadline that fires even when the scene never calls back. */
let booted=false, degraded=false, bootTimer=null, bootSource='scene';
/* Isolated mode boots instantly (there is no entrance to watch), so the title is
 * closed outright instead of fading: a capture taken mid-fade carries a ghost of
 * "Navi online" over the chat. Production keeps the 900 ms hold and the fade
 * (BUG-029); the stylesheet's `display:flex` would beat the `hidden` attribute, so
 * both are set. */
const instantBoot=host.isolated===true;
function hideBootOverlay() {
  const overlay=element('boot');
  if (!overlay) return;
  overlay.hidden=true;
  overlay.style.display='none';
}
function markBooted(source) {
  /* A late scene or failure callback may relabel a boot the deadline forced, but
   * the deadline must never overwrite a stage that already reported (BUG-029). */
  if (booted) {
    if (source!=='deadline'&&bootSource==='deadline') bootSource=source;
    return;
  }
  bootSource=source;
  booted=true;
  clearTimeout(bootTimer);
  document.body.classList.remove('booting');
  document.body.classList.add('booted');
  if (instantBoot) hideBootOverlay();
  syncFrame();
  setMotion();
}
function degrade(reason) {
  const first=!degraded;
  degraded=true;
  document.body.classList.add('degraded');
  const field=element('field');
  if (field) field.dataset.renderer=window.naviScene?'fallback':'missing';
  const chat=document.querySelector('.chat');
  if (chat) chat.style.opacity='1';  // a dead stage must never hide the chat
  markBooted('degraded');
  if (first) record('warn','stage unavailable: '+reason);
}
addEventListener('error',event=>{
  record('error',event.error?.message||event.message||'script error',event.error?.stack);
  markBooted('error');
});
addEventListener('unhandledrejection',event=>{
  const reason=event.reason;
  record('error',reason?.message||String(reason||'unhandled rejection'),reason?.stack);
  markBooted('rejection');
});
addEventListener('navi:degraded',event=>{
  degrade((event.detail&&event.detail.reason)||'the stage reported a failure');
});

/* The scene re-evaluates devicePixelRatio inside resize() and clamps it to
 * window.naviDprCap, so the station owns the cap and moving between displays
 * keeps the stage sharp without unbounded overdraw. */
const DPR_CAP=2;
function syncPixelRatio() {
  window.naviDprCap=DPR_CAP;
  const api=window.naviScene;
  if (typeof api?.setPixelRatio==='function') api.setPixelRatio(Math.min(devicePixelRatio||1,DPR_CAP));
}
addEventListener('resize',syncPixelRatio);

function setMotion(){window.naviScene?.setMotion(motion);document.body.classList.toggle('still',!motion);}
/* The scene follows the OS query on its own; the station mirrors the same value
 * so `body.still` and the scene never disagree about motion. */
reducedMotion.addEventListener('change',event=>{ motion=!event.matches; setMotion(); });
/* Isolated-only: force the degraded path so the recovery contract is testable. */
if (host.webglFail) {
  window.naviScene?.degrade?.('NAVI_FORCE_WEBGL_FAIL');
  degrade('forced WebGL failure (NAVI_FORCE_WEBGL_FAIL)');
}
/* The scene may have fallen back before this file ran (its event would be
 * missed), so report the state it left on the canvas as well. */
const field=element('field');
if (field && (field.dataset.renderer==='fallback' || (window.naviScene && !window.naviScene.renderer))) {
  degrade('the stage could not create a WebGL context');
}

/* Boot is never blocked on the stage: the deadline fires even when the bundle is
 * missing, and a missing bundle is reported as a degradation, not a blank page. */
bootTimer=setTimeout(()=>{
  if (!window.naviScene) degrade('the stage bundle did not load');
  markBooted('deadline');
},1200);
/* `instant` skips the entrance for tests only: in production the scene holds the
 * title and flies in, and the station waits for its callback before marking the
 * page booted. */
window.naviScene?.boot(()=>markBooted('scene'),instantBoot);
syncPixelRatio();
send({type:'ready'});poll();
refreshHarnesses();
