import { sanitizeUrl } from "../../../packages/navi/browser/content/collector.ts";
let attached = null, attachedOrigin = null, port = null, connected = false, waiting = false, lastSnapshot = 0;
async function revoke(){attached=null;const origin=attachedOrigin;attachedOrigin=null;if(origin)await browser.permissions.remove({origins:[origin]});}
const host = 'dev.navi.station';
function connect() {
  if(port)return;
  try {
    port=browser.runtime.connectNative(host);
    port.onDisconnect.addListener(()=>{port=null;connected=false;waiting=false;setTimeout(connect,2000);});
    port.onMessage.addListener(async reply=>{
      waiting=false;connected=reply.ok;
      if(reply.ok&&reply.result){const item=reply.result;let result;if(item.expires<Date.now())result={ok:false,error:'Action expired before delivery'};else try{result=await execute(item.action);}catch(error){result={ok:false,error:error.message};}synchronize({...result,ticket:item.ticket});}
    });
    synchronize();
  }catch{port=null;setTimeout(connect,2000);}
}
async function snapshot() {
  const tabs=(await browser.tabs.query({})).slice(0,40).map(tab=>({id:tab.id,title:(tab.title||'').slice(0,120),url:sanitizeUrl(tab.url||''),active:tab.active}));
  let page={};
  if(attached!==null)try{page=await browser.tabs.sendMessage(attached,{type:'observe'});}catch{attached=null;}
  return {tabs,attached,...page};
}
async function synchronize(result) {
  if(!port||waiting)return;
  waiting=true;
  try {
    const params={};
    if(result)params.result=result;
    if(Date.now()-lastSnapshot>1200||result){params.snapshot=await snapshot();lastSnapshot=Date.now();}
    port.postMessage({op:'browser_sync',params});
  } catch { waiting=false; }

}
async function execute(action) {
  if(action.type==='popup'){await browser.action.openPopup();return {ok:true,message:'Navi browser popup opened'};}
  if(action.type==='attach') {
    const tab=await browser.tabs.get(action.tab);
    if(!/^https?:/.test(tab.url||''))throw new Error('Select a regular web page');
    await browser.scripting.executeScript({target:{tabId:tab.id},files:['content.js']});
    attached=tab.id;attachedOrigin=new URL(tab.url).protocol+'//'+new URL(tab.url).hostname+'/*';lastSnapshot=0;
    await browser.tabs.update(tab.id,{active:true});await browser.windows.update(tab.windowId,{focused:true});
    return {ok:true,message:'Named browser tab attached'};
  }
  if(action.type==='tab'){await browser.tabs.update(action.tab,{active:true});const tab=await browser.tabs.get(action.tab);await browser.windows.update(tab.windowId,{focused:true});return {ok:true,message:'Browser tab selected'};}
  if(action.tab!==attached||attached===null)throw new Error('This tab is not attached for control');
  const tab=await browser.tabs.get(attached);await browser.tabs.update(attached,{active:true});await browser.windows.update(tab.windowId,{focused:true});return browser.tabs.sendMessage(attached,action);
}
browser.tabs.onRemoved.addListener(id=>{if(id===attached)revoke();});
browser.tabs.onUpdated.addListener((id,changes)=>{if(id===attached&&(changes.status==='loading'||changes.url))revoke();});
browser.runtime.onMessage.addListener(async(message,sender)=>{
  if(sender.tab || sender.id!==browser.runtime.id || sender.url!==browser.runtime.getURL('popup.html'))return undefined;
  if(message.type==='status')return {connected,attached};
  if(message.type==='detach'){await revoke();return {connected,attached};}
  if(message.type==='attach'){
    const tabs=await browser.tabs.query({active:true,currentWindow:true});const tab=tabs[0];
    if(!tab||!/^https?:/.test(tab.url||''))throw new Error('Select a regular web page first');
    await browser.scripting.executeScript({target:{tabId:tab.id},files:['content.js']});attached=tab.id;attachedOrigin=new URL(tab.url).protocol+'//'+new URL(tab.url).hostname+'/*';lastSnapshot=0;
    return {connected,attached};
  }
  return undefined;
});
setInterval(()=>synchronize(),350);connect();
