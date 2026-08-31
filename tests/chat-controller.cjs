// node tests/chat-controller.cjs — deterministic subscription/snapshot/tab races.
const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm');
const R=require('../src/web/log.js');
const sockets=[], requests=[], timers=[], trace=[];
class Socket {
  constructor(url){this.url=url;this.readyState=1;sockets.push(this);trace.push('subscribe');}
  close(){this.readyState=3;this.onclose?.();}
  emit(event){this.onmessage?.({data:JSON.stringify(event)});}
}
const elements=new Map();
const node=()=>({value:'',style:{},textContent:'',appendChild(n){this.textContent+=n.textContent||'';}});
const ctx=vm.createContext({ReveLog:R,WebSocket:Socket,Promise,Map,Set,JSON,console,
  current:null,ws:null,bots:[{id:'a',name:'A',status:'ready'},{id:'b',name:'B',status:'ready'}],
  location:{protocol:'http:',host:'fixture'},TOKEN:'fixture',PAGE:80,
  document:{getElementById(id){if(!elements.has(id))elements.set(id,node());return elements.get(id);}},
  api(url){trace.push('snapshot');return new Promise((resolve,reject)=>requests.push({url,resolve,reject}));},
  requestAnimationFrame(){},setTimeout(fn){timers.push(fn);},setBusy(){},setRoute(){},resetTranscript(){},paintHead(){},closeDrawers(){},
  el:(t,c,text)=>({textContent:text}),loadBots:async()=>{},loadSkills:async()=>{},loadRoutines:async()=>{},
});
vm.runInContext(fs.readFileSync(require('node:path').join(__dirname,'../src/web/chat.js'),'utf8'),ctx);
const e=(id,text,seq=0)=>({id,seq,type:'message',display:{audience:'chat',run_id:'run'},message:{role:'assistant',content:[{type:'text',text}],stopReason:'stop'}});
const rows=id=>vm.runInContext(`chatLog(${JSON.stringify(id)}).records()`,ctx);
const snapshot=(log_id,records)=>({log_id,records,has_more:false,oldest_seq:1,operation_id:null});
(async()=>{
  const first=ctx.select('a');
  assert.equal(requests.length,0,'wait for subscribed log identity before requesting a snapshot');
  sockets[0].emit({type:'hello',log_id:'session-a'});
  assert.deepEqual(trace,['subscribe','snapshot']);
  sockets[0].emit({type:'entry_draft',entry:e('reply','old'),order:2,version:1});
  requests.shift().resolve(snapshot('session-a',[{entry:e('reply','latest'),status:'streaming',order:2,revision:10}]));
  await first;
  assert.equal(R.text(rows('a')[0].entry.message.content),'latest','buffered old draft must not shrink snapshot');
  sockets[0].emit({type:'entry_added',entry:e('reply','latest complete',3)});
  assert.equal(rows('a')[0].status,'committed');
  ctx.document.getElementById('text').value='A draft';
  const second=ctx.select('b');
  sockets[1].emit({type:'hello',log_id:'session-b'});
  const bRequest=requests.shift();
  sockets[0].emit({type:'entry_added',entry:e('poison','wrong bot',99)});
  assert.equal(rows('b').length,0);
  const third=ctx.select('a');
  assert.equal(ctx.document.getElementById('text').value,'A draft','compose drafts belong to their bot');
  sockets[2].emit({type:'hello',log_id:'session-a'});
  bRequest.resolve(snapshot('session-b',[{entry:e('late-b','late',1),status:'committed',order:1,revision:0}]));
  requests.shift().resolve(snapshot('session-a',[{entry:e('reply','latest complete',3),status:'committed',order:3,revision:0}]));
  await Promise.all([second,third]);
  assert.equal(ctx.current,'a');assert.equal(rows('a').length,1);assert.equal(rows('b').length,0);
  // A recreated bot has a new durable log identity; old history/drafts cannot leak.
  sockets[2].close();
  timers.shift()();
  sockets[3].emit({type:'hello',log_id:'replacement-a'});
  requests.shift().resolve(snapshot('replacement-a',[{entry:e('new','new bot',1),status:'committed',order:1,revision:0}]));
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(rows('a').length,1);assert.equal(rows('a')[0].entry.id,'new');
  assert.equal(ctx.document.getElementById('text').value,'');
  sockets[2].emit({type:'entry_added',entry:e('old','old incarnation',100)});
  assert.equal(rows('a').length,1);
  console.log('chat subscription, snapshot, switching, reconnect and incarnation races: passed');
})().catch(error=>{console.error(error);process.exitCode=1;});
