// Browser adapter. Every transcript update is log -> projection -> renderer.
const logs = new Map();
const logIds = new Map();
const rowCache = new Map();
const activityOpen = new Set();
const outputOpen = new Set();
const runningBots = new Set();
const disconnected = new Set();
const faults = new Map();
const composeDrafts = new Map();
let connection = 0;
let streamReady = Promise.resolve();
let sending = false;
function chatLog(id) {
  if (!logs.has(id)) logs.set(id, new ReveLog.Log());
  return logs.get(id);
}

function renderActivity(group) {
  const details = el('details', 'activity');
  const failed = group.tools.filter(t => t.failed);
  if (failed.length) details.classList.add('has-errors');
  details.open = activityOpen.has(group.id);
  details.ontoggle = () => { if (details.open) activityOpen.add(group.id); else activityOpen.delete(group.id); };
  const summary = el('summary', 'activity-summary');
  summary.appendChild(el('span', 'activity-title', 'Activity'));
  summary.appendChild(el('span', 'activity-count', group.tools.length + (group.tools.length === 1 ? ' action' : ' actions')));
  if (failed.length) summary.appendChild(el('span', 'activity-error', failed.length + ' failed'));
  details.appendChild(summary);
  details.appendChild(el('div', 'activity-description', ReveLog.summary(group.tools)));
  for (const tool of group.tools) {
    const item = el('details', 'activity-action' + (tool.failed ? ' failed' : ''));
    item.open = outputOpen.has(tool.id);
    item.ontoggle = () => { if (item.open) outputOpen.add(tool.id); else outputOpen.delete(tool.id); };
    const line = el('summary');
    line.appendChild(el('span', 'activity-mark', tool.running ? '…' : tool.failed ? '!' : tool.uncertain ? '?' : '✓'));
    line.appendChild(el('span', 'activity-name', tool.name));
    const preview = el('span', 'activity-args', argsPreview(tool.name, tool.args));
    line.appendChild(preview);
    item.appendChild(line);
    if (tool.text) {
      const pre = el('pre', 'activity-output');
      pre.textContent = tool.text;
      item.appendChild(pre);
    } else item.appendChild(el('div', 'activity-description', tool.running ? 'Running…' : tool.uncertain ? 'Awaiting a committed result.' : 'No output.'));
    details.appendChild(item);
  }
  track(details, 'activity');
  if (failed.length) {
    const latest = failed[failed.length - 1];
    track(el('div', 'activity-error-line', latest.name + ': ' + (latest.text || 'interrupted').replace(/\s+/g, ' ').slice(0, 180)), 'notice');
  }
}

function paintLog(bot, rows) {
  if (!rows.length) {
    track(el('div', 'empty', 'Message ' + (bot ? bot.name : 'this bot') + ' to get started.'), 'empty');
    return;
  }
  for (const row of rows) {
    const signature = JSON.stringify(row) + (row.kind === 'secret' ? '' : JSON.stringify([bot && bot.name, bot && bot.avatar]));
    const cached = rowCache.get(row.id);
    if (cached && cached.signature === signature) { items.push(...cached.items); continue; }
    const start = items.length;
    let node;
    if (row.kind === 'activity') renderActivity(row);
    else if (row.kind === 'secret') addSecretAsk(row.args, {bot, running:true, id:row.id});
    else if (row.kind === 'user') {
      const user = unwrapUser(row.text);
      if (row.from || user.kind === 'agent') {
        const who = row.fromName || user.name;
        node = addMessage('start', 'tinted', who, user.text, null, row.from || user.id);
      } else node = addMessage('end', 'default', 'you', user.text, null, 'you');
    } else if (row.kind === 'assistant') {
      // Partial and committed text use exactly the same Markdown renderer.
      node = addMessage('start', 'muted', bot ? bot.name : 'bot', row.text, bot && bot.avatar, bot && bot.id);
    } else addMarker(row.text);
    if (node && (row.label || row.status === 'cancelled')) {
      node.querySelector('.msg-col').appendChild(el('div', 'draft-label', row.label || 'Cancelled before being acted on'));
    }
    rowCache.set(row.id, {signature, items:items.slice(start)});
  }
}

function renderStatus(id) {
  if (current !== id) return;
  if (faults.has(id)) {
    setBusy(id, false);
    document.getElementById('status').textContent = 'Session error: ' + faults.get(id);
    return;
  }
  if (disconnected.has(id)) {
    setBusy(id, false);
    document.getElementById('status').textContent = 'Disconnected — reconnecting…';
    return;
  }
  const rows = ReveLog.project(chatLog(id).records());
  const active = rows.filter(r => (r.kind === 'tool' || r.kind === 'secret') && r.running).at(-1);
  const working = runningBots.has(id) || !!active || chatLog(id).records().some(r => r.status === 'streaming');
  setBusy(id, working);
  const status = document.getElementById('status');
  status.textContent = '';
  if (working) status.appendChild(el('span', 'shimmer', 'Working…' + (active ? ' ' + ReveLog.action(active) : '')));
}

function rebuild(bot, keepScroll) {
  if (!current) return;
  const log = logEl(), oldHeight = log.scrollHeight, oldTop = log.scrollTop;
  items = [];
  silent = true;
  const rows = ReveLog.group(ReveLog.project(chatLog(current).records()));
  const visibleIds = new Set(rows.map(row => row.id));
  for (const id of rowCache.keys()) if (!visibleIds.has(id)) rowCache.delete(id);
  paintLog(bot, rows);
  silent = false;
  flush();
  if (keepScroll && !pin) log.scrollTop = oldTop + log.scrollHeight - oldHeight;
  renderStatus(current);
}
function queueLogRender(id) {
  if (current !== id || queueLogRender.queued) return;
  queueLogRender.queued = true;
  requestAnimationFrame(() => {
    queueLogRender.queued = false;
    if (current) rebuild(bots.find(b => b.id === current), true);
  });
}

async function refreshLog(id, expectedId, valid) {
  const data = await api('/api/bots/' + encodeURIComponent(id) + '/messages?limit=' + PAGE);
  if (!valid()) return;
  if (!Array.isArray(data.records) || typeof data.log_id !== 'string') throw new Error('Invalid log snapshot');
  if (data.log_id !== expectedId) throw new Error('Bot log changed; reconnecting');
  if (logIds.has(id) && logIds.get(id) !== data.log_id) {
    logs.delete(id); rowCache.clear(); composeDrafts.delete(id);
    if (current === id) document.getElementById('text').value = '';
  }
  logIds.set(id, data.log_id);
  const log = chatLog(id);
  const received = new Set((data.records || []).map(r => r.entry.id));
  const outstanding = log.records().filter(r => (r.status === 'streaming' || r.status === 'accepted') && !received.has(r.entry.id));
  log.merge(data.records || []);
  for (const record of outstanding) {
    try { log.upsert(await api('/api/bots/' + encodeURIComponent(id) + '/messages/' + encodeURIComponent(record.entry.id))); }
    catch (error) {
      if (error.status !== 404) throw error;
      log.upsert({...record, revision:0, status:record.status === 'accepted' ? 'cancelled' : 'interrupted'});
    }
  }
  if (!valid()) return;
  if (data.operation_id) runningBots.add(id); else runningBots.delete(id);
  if (current === id) {
    hasMore = !!data.has_more;
    const committed = chatLog(id).records().filter(r => r.status === 'committed');
    oldestSeq = committed.length ? committed[0].order : null;
    queueLogRender(id);
  }
}

async function loadOlder() {
  if (!current || !hasMore || loadingOlder || !oldestSeq) return;
  loadingOlder = true;
  const id = current, epoch = connection, expectedId = logIds.get(current);
  try {
    const data = await api('/api/bots/' + encodeURIComponent(id) + '/messages?limit=' + PAGE + '&before=' + oldestSeq);
    if (current !== id || connection !== epoch || data.log_id !== expectedId) return;
    chatLog(id).merge(data.records || []);
    if (current === id && connection === epoch) {
      hasMore = !!data.has_more;
      oldestSeq = data.oldest_seq || oldestSeq;
      rebuild(bots.find(b => b.id === id), true);
    }
  } finally { if (connection === epoch) loadingOlder = false; }
}

function connectLog(id, epoch) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket((location.protocol === 'https:' ? 'wss:' : 'ws:') + '//' + location.host +
      '/api/bots/' + encodeURIComponent(id) + '/events?token=' + encodeURIComponent(TOKEN));
    ws = socket;
    const valid = () => current === id && connection === epoch && ws === socket;
    let ready = false, buffered = [], syncing = false, identity = null;
    function apply(event) {
      if (event.type === 'run_start' || event.type === 'run_resume') runningBots.add(id);
      if (event.type === 'run_end') { runningBots.delete(id); loadBots().catch(console.error); }
      if (event.type === 'fault') { runningBots.delete(id); faults.set(id, event.message); }
      if (chatLog(id).event(event)) queueLogRender(id);
      else renderStatus(id);
    }
    async function sync() {
      if (syncing) return;
      syncing = true; ready = false;
      try {
        await refreshLog(id, identity, valid);
        if (!valid()) { resolve(); return; }
        ready = true;
        disconnected.delete(id); faults.delete(id);
        const events = buffered; buffered = [];
        for (const event of events) apply(event);
        resolve();
      } catch (error) { reject(error); socket.close(); }
      finally { syncing = false; }
    }
    socket.onopen = () => {}; // wait for the subscribed session's identity
    socket.onmessage = message => {
      if (!valid()) return;
      const event = JSON.parse(message.data);
      if (event.type === 'hello') { identity = event.log_id; sync(); return; }
      if (event.type === 'lagged') { sync(); return; }
      if (!ready) buffered.push(event); else apply(event);
    };
    socket.onerror = () => { if (!ready) reject(new Error('Could not connect to the bot log')); };
    socket.onclose = () => {
      if (!valid()) return;
      if (!ready) reject(new Error('Bot log connection closed'));
      chatLog(id).interrupt(); runningBots.delete(id); disconnected.add(id); queueLogRender(id);
      const profile = bots.find(bot => bot.id === id);
      if (!profile || profile.status !== 'ready') return;
      setTimeout(() => {
        if (valid()) streamReady = connectLog(id, epoch).catch(error => { if (current === id) document.getElementById('status').textContent = error.message; });
      }, 1000);
    };
  });
}

async function select(id, fromRoute) {
  if (!id) return;
  if (current === id) { if (!fromRoute) setRoute(id); return; }
  if (current) composeDrafts.set(current, document.getElementById('text').value);
  current = id; const epoch = ++connection;
  document.getElementById('text').value = composeDrafts.get(id) || '';
  document.getElementById('text').style.height = 'auto';
  if (!fromRoute) setRoute(id);
  if (ws) { ws.onclose = null; ws.close(); }
  rowCache.clear(); loadingOlder = false; resetTranscript();
  paintHead(bots.find(b => b.id === id));
  queueLogRender(id);
  const profile = bots.find(bot => bot.id === id);
  if (profile && profile.status !== 'ready') {
    document.getElementById('status').textContent = profile.profile_error || 'Bot is unavailable';
    return;
  }
  streamReady = connectLog(id, epoch);
  loadBots().catch(console.error);
  loadSkills(id).catch(console.error);
  loadRoutines().catch(console.error);
  try { await streamReady; }
  catch (error) { if (current === id) document.getElementById('status').textContent = error.message; }
}
