// Exercise the real metadata renderer without a browser or network.
// Run: node tests/profile-ui.cjs
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const html = fs.readFileSync(require('node:path').join(__dirname, '../src/web/index.html'), 'utf8');
function extract(name) {
  let start = html.indexOf('async function ' + name + '(');
  if (start < 0) start = html.indexOf('function ' + name + '(');
  assert(start >= 0, name);
  return html.slice(start, html.indexOf('\n}', start) + 2);
}
const element = () => ({ value:'', children:[], textContent:'', appendChild(n) { this.children.push(n); return n; } });
const nodes = new Map();
let profiles = [{id:'miku', name:'Miku', title:'Old'}];
let header;
const context = vm.createContext({
  document:{getElementById(id) { if (!nodes.has(id)) nodes.set(id, element()); return nodes.get(id); }},
  current:'miku', toolsStarted:false, bots:[],
  api:async () => ({bots:profiles}), ensureHouseEvents(){},
  el:(tag, cls, text) => ({...element(), textContent:text || ''}),
  avatar:element, modelBadge:() => '',
  paintHead:profile => {header = profile;},
});
vm.runInContext(extract('groupOf') + '\n' + extract('loadBots'), context);
(async () => {
  await context.loadBots(); assert.equal(header.name, 'Miku');
  profiles = [{id:'miku', name:'Music', title:'Composer', avatar:'blue:blob'}];
  await context.loadBots(); assert.equal(header.name, 'Music'); assert.equal(header.avatar, 'blue:blob');
  profiles = [{...profiles[0], profile_error:'invalid JSON'}];
  await context.loadBots(); assert.equal(header.profile_error, 'invalid JSON');
  assert(JSON.stringify(nodes.get('bots')).includes('Profile error: invalid JSON'));
  console.log('profile UI refresh: passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
