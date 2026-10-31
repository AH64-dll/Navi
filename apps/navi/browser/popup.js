async function update(type){try{const status=await browser.runtime.sendMessage({type});document.getElementById('status').textContent=(status.connected?'Local station connected':'Local station disconnected')+(status.attached!==null?' · tab attached':'');}catch(error){document.getElementById('status').textContent=error.message;}}
let origin;
const attach=document.getElementById('attach');attach.disabled=true;
browser.tabs.query({active:true,currentWindow:true}).then(tabs=>{const url=new URL(tabs[0].url);if(!['http:','https:'].includes(url.protocol))throw new Error('Select a regular web page');origin=url.protocol+'//'+url.hostname+'/*';attach.disabled=false;attach.textContent='Attach '+url.hostname;}).catch(error=>{document.getElementById('status').textContent=error.message;});
attach.onclick=()=>{browser.permissions.request({origins:[origin]}).then(granted=>{if(granted)return update('attach');document.getElementById('status').textContent='Page permission declined';}).catch(error=>{document.getElementById('status').textContent=error.message;});};
document.getElementById('detach').onclick=()=>update('detach');update('status');
