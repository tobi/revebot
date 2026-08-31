// node tests/chat-log.cjs — real log model and real Markdown functions, no network.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const R = require('../src/web/log.js');
const html = fs.readFileSync(path.join(__dirname, '../src/web/index.html'), 'utf8');
const chat = fs.readFileSync(path.join(__dirname, '../src/web/chat.js'), 'utf8');
const entry = (id, text, audience='chat', seq=0) => ({id, seq, type:'message', display:{run_id:'run', audience}, message:{role:'assistant', content:[{type:'text', text}], stopReason:'stop'}});
const committed = e => ({entry:e, status:'committed', order:e.seq, revision:0});
const draft = (e, version) => ({type:'entry_draft', entry:e, order:2, version});

// Streaming, settlement, snapshot and pagination feed the same keyed log.
const log = new R.Log();
log.event(draft(entry('reply', '**Hello**'), 2));
assert.equal(R.project(log.records())[0].text, '**Hello**');
log.event({type:'entry_added', entry:entry('reply', '**Hello**', 'chat', 10)});
log.merge([committed(entry('reply', '**Hello**', 'chat', 10))]);
log.event(draft(entry('reply', 'stale'), 1));
assert.equal(log.records().length, 1);
assert.equal(R.project(log.records())[0].text, '**Hello**');
const fresh = new R.Log(); fresh.merge(log.records());
assert.deepEqual(R.project(fresh.records()), R.project(log.records()));
// No history-dependent visibility: a steer doesn't turn private prose public.
log.merge([committed({id:'steer', seq:11, type:'message', message:{role:'user', content:'another question'}}), committed(entry('internal', 'private prose', 'internal', 12))]);
assert(!R.project(log.records()).some(row => row.text === 'private prose'));
// An intent, even a blocked one, is never rendered as a delivered notice.
log.merge([committed({id:'intent', seq:13, type:'message', message:{role:'assistant', content:[{type:'toolCall', name:'SendUserMessage', arguments:{text:'NOT SENT'}}]}})]);
assert(!R.project(log.records()).some(row => row.text === 'NOT SENT'));
const notice = {id:'notice', seq:20, type:'custom', customType:'user_notice', data:{text:'Delivered'}};
log.event({type:'entry_accepted', entry:notice, order:14});
log.event({type:'entry_added', entry:notice});
log.merge([committed(notice)]);
assert.equal(R.project(log.records()).filter(row => row.text === 'Delivered').length, 1);
// Draft revisions stop buffered old frames from shrinking a snapshot's text.
const resumed = new R.Log();
resumed.event(draft(entry('draft', 'current partial'), 10));
resumed.event(draft(entry('draft', 'old'), 3));
assert.equal(R.project(resumed.records())[0].text, 'current partial');
resumed.interrupt();
assert.equal(R.project(resumed.records())[0].label, 'Interrupted draft · not persisted');
resumed.event({type:'entry_added', entry:{...entry('draft', '', 'chat', 30), message:{role:'assistant', content:[], stopReason:'aborted', errorMessage:'Stopped'}}});
assert.equal(R.project(resumed.records())[0].text, 'current partial');
assert(R.project(resumed.records())[0].label.includes('not persisted'));
// All tool records in a run live in one disclosure, including across notices.
const tools = Array.from({length:12}, (_,i) => ({id:'t'+i, kind:'tool', run:'run', name:i<7?'read':'ls', args:{path:'/workspace/file'}, text:'large output', running:false, failed:false}));
const groups = R.group([...tools.slice(0,5), {id:'n', kind:'assistant', text:'Update'}, ...tools.slice(5)]);
assert.equal(groups.filter(row => row.kind === 'activity').length, 1);
assert.equal(R.summary(groups[0].tools), 'Read 7 files · Listed 5 directories');
assert.equal(R.action({name:'bash', args:{description:'Running tests'}}), 'Running tests');
assert.equal(R.group([{kind:'secret',id:'ask',run:'run'}, ...tools])[0].kind, 'secret');

function extract(source, name) {
  let start = source.indexOf('async function '+name+'(');
  if (start < 0) start = source.indexOf('function '+name+'(');
  assert(start >= 0, name);
  return source.slice(start, source.indexOf('\n}', start)+2);
}
class Element {
  constructor(tag, text='') { this.tag=tag; this.children=[]; this._text=text; this.className=''; this.dataset={}; this.style={}; }
  appendChild(node) { node.parentElement=this; this.children.push(node); return node; }
  get textContent() { return this._text + this.children.map(c=>c.textContent).join(''); }
  set textContent(text) { this._text=String(text); this.children=[]; }
  querySelector(selector) { return this.children.flatMap(c=>[c,...c.descendants()]).find(n=>n.className.split(' ').includes(selector.slice(1))) || null; }
  descendants() { return this.children.flatMap(c=>[c,...c.descendants()]); }
  get classList() { return {add:c=>this.className+=' '+c, contains:c=>this.className.split(' ').includes(c)}; }
}
const document = {createElement:tag=>new Element(tag), createTextNode:text=>new Element('#text',text)};
const ctx = vm.createContext({document, URL, items:[], rowCache:new Map(), avatar:()=>new Element('avatar'), track:(node)=>{ctx.items.push({node});}, silent:true, scheduleFlush(){}, ReveLog:R});
for (const name of ['el','safeHttp','appendInline','formatText','addMessage','unwrapUser']) vm.runInContext(extract(html,name),ctx);
vm.runInContext(extract(chat,'paintLog'),ctx);
const markdown = '# Heading\n\n- first\n- second\n\n**bold** and `code`\n\n```rust\nfn main() {\n  println!("hello");\n';
for (let n=1; n<=markdown.length; n++) ctx.formatText(markdown.slice(0,n));
const dom = ctx.formatText(markdown);
const tags = [dom,...dom.descendants()].map(n=>n.tag);
for (const tag of ['h1','ul','li','strong','code','pre']) assert(tags.includes(tag), tag);
assert(dom.textContent.includes('println!("hello");'));
assert(![ctx.formatText('<script>alert(1)</script>'),...ctx.formatText('<script>alert(1)</script>').descendants()].some(n=>n.tag==='script'));
// Actual paintLog calls the same Markdown renderer for partial and settled rows.
const bot={id:'bot',name:'Bot'};
ctx.paintLog(bot,[{id:'m',kind:'assistant',status:'streaming',text:markdown}]);
const partialBody = ctx.items[0].node.textContent;
ctx.items=[];
ctx.paintLog(bot,[{id:'m',kind:'assistant',status:'committed',text:markdown}]);
assert.equal(ctx.items[0].node.textContent, partialBody);
// Parse the embedded page, not just individual functions.
const page = html.replace('/*{{BLOUB}}*/',fs.readFileSync(path.join(__dirname,'../src/web/bloub.js'),'utf8')).replace('/*{{LOG}}*/',fs.readFileSync(path.join(__dirname,'../src/web/log.js'),'utf8')).replace('/*{{CHAT}}*/',chat);
new vm.Script(page.slice(page.lastIndexOf('<script>')+8, page.lastIndexOf('</script>')));
console.log('chat log, compact activity, streaming Markdown and page syntax: passed');
