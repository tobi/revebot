import { BLOUB } from "./lib/bloub.mjs";
import * as ReveLog from "./lib/log.mjs";
import { formatText as renderMarkdown } from "./lib/markdown.mjs";
import { emitServerState, nextRequestId } from "./events.mjs";
import "./components/reve-feed.mjs";
import "./components/reve-autocomplete.mjs";
import "./components/reve-attachment.mjs";
import "./components/reve-composer.mjs";
const TOKEN = document.querySelector('meta[name="reve-token"]')?.content || "";
const headers = { "Authorization": "Bearer " + TOKEN, "Content-Type": "application/json" };
let current = null;
let bots = [];
let ws = null;
let houseWs = null;
let frozenConversation = null;
let busy = {};
let statuslines = {};
const PAGE = 80;
const feed = document.getElementById("feed");
const composer = document.getElementById("compose");
const autocomplete = document.getElementById("ac");
const attachmentPreviewRequests = new WeakMap();
composer.autocomplete = autocomplete;

function logEl() { return feed.logElement; }

function track(node, kind) { return feed.track(node, kind); }

function scheduleFlush() { feed.scheduleFlush(); }

function flush() { feed.flush(); }

function resetTranscript() { feed.reset(); }

function requestOwner(path) {
  const match = /^\/api\/bots\/([^/?]+)/.exec(path);
  if (!match) return null;
  try {
    return { botId: decodeURIComponent(match[1]) };
  } catch {
    return { botId: match[1] };
  }
}

async function api(path, opt = {}, context = {}) {
  const requestId = context.requestId || nextRequestId("http");
  const method = opt.method || "GET";
  const owner = context.owner || requestOwner(path);
  let emitted = false;
  try {
    const res = await fetch(path, { headers, ...opt });
    const data = res.status === 204 ? null : await res.json().catch(() => ({}));
    if (!res.ok) {
      const error = new Error((data.error && data.error.message) || data.error || res.statusText);
      error.status = res.status;
      emitted = true;
      emitServerState({
        source: "http", requestId, topic: context.topic || "http", owner,
        path, method, ok: false, status: res.status, error: error.message,
      });
      throw error;
    }
    emitted = true;
    emitServerState({
      source: "http", requestId, topic: context.topic || "http", owner,
      path, method, ok: true, status: res.status, data,
    });
    return data;
  } catch (error) {
    if (!emitted) {
      emitServerState({
        source: "http", requestId, topic: context.topic || "http", owner,
        path, method, ok: false, status: 0,
        error: error.name === "AbortError" ? "Request cancelled" : error.message || String(error),
      });
    }
    throw error;
  }
}

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text) n.textContent = text;
  return n;
}

function avatar(id, spec, size) {
  const wrap = el("div", "av" + (size > 48 ? " lg" : size < 36 ? " sm" : ""));
  wrap.dataset.bot = id || "";
  wrap.innerHTML = BLOUB.svg(spec, id, size || 42);
  return wrap;
}

function setBusy(id, on) {
  if (!id) return;
  if (on) busy[id] = true;
  else delete busy[id];
  paintBusy();
}

function paintBusy() {
  document.querySelectorAll("#bots .bot").forEach((n) => {
    const av = n.querySelector(".av");
    if (av) av.classList.toggle("working", !!busy[n.dataset.id]);
  });
  const head = document.getElementById("head-av");
  if (head) head.classList.toggle("working", !!busy[current]);
}

function ensureHouseEvents() {
  if (houseWs && (houseWs.readyState === 0 || houseWs.readyState === 1)) return;
  houseWs = new WebSocket((location.protocol === "https:" ? "wss:" : "ws:") + "//" + location.host +
    "/api/events?token=" + encodeURIComponent(TOKEN));
  houseWs.onmessage = (ev) => {
    const e = JSON.parse(ev.data);
    const botId = e.bot_id || e.botId || null;
    emitServerState({
      source: "websocket", requestId: null, topic: "house-event",
      owner: botId ? { botId } : null, channel: "house", ok: true, data: e,
    });
    if (e.type === "bot_busy") setBusy(e.bot_id || e.botId, !!e.busy);
    if (e.type === "statusline") {
      const id = e.bot_id || e.botId;
      statuslines[id] = e.text || "";
      if (id === current) composer.setStatusline(statuslines[id]);
    }
    if (e.type === "roster_changed" || e.type === "lagged") loadBots().catch(console.error);
  };
  houseWs.onopen = () => loadBots().catch(console.error);
  houseWs.onclose = () => { setTimeout(ensureHouseEvents, 1500); };
}

function modelBadge(spec) {
  if (!spec) return "";
  const id = spec.split("/").pop() || spec;
  const lower = id.toLowerCase();
  if (lower.includes("grok")) return "Grok";
  if (lower.includes("claude") || lower.includes("opus")) return "Opus";
  if (lower.includes("gemini")) return "Gemini";
  if (lower.includes("gpt") || lower.includes("openai")) return "GPT";
  if (lower.includes("deepseek")) return "DeepSeek";
  if (lower.includes("qwen")) return "Qwen";
  if (lower.includes("glm")) return "GLM";
  return id.replace(/-\d.*$/, "").slice(0, 14);
}

function contentParts(content) {
  if (content == null || content === "") return [];
  if (typeof content === "string") {
    const t = content.trim();
    if (!t || t === "[]") return [];
    if (t.startsWith("[") || t.startsWith("{")) {
      try { return contentParts(JSON.parse(t)); } catch (_) {}
    }
    return [{ type: "text", text: content }];
  }
  if (Array.isArray(content)) return content.filter(Boolean);
  if (typeof content === "object") {
    if (content.type || content.text) return [content];
    if (content.content) return contentParts(content.content);
  }
  return [];
}

function textOf(content) {
  return contentParts(content)
    .filter((p) => p && (p.type === "text" || !p.type) && p.text)
    .map((p) => p.text)
    .join("\n")
    .trim();
}

function asArgs(args) {
  if (!args) return {};
  if (typeof args === "string") {
    try {
      const parsed = JSON.parse(args);
      if (parsed && typeof parsed === "object") return parsed;
    } catch (_) {}
    return { value: args };
  }
  if (typeof args === "object") return args;
  return {};
}

function argsPreview(name, args) {
  args = asArgs(args);
  const prefer = name === "bash" || name === "example"
    ? ["command", "path", "name", "id", "pattern", "text"]
    : name === "AskUserForSecret"
    ? ["title", "env", "reason", "description"]
    : ["name", "path", "id", "command", "pattern", "text"];
  for (const k of prefer) {
    if (typeof args[k] === "string" && args[k]) {
      const v = args[k].replace(/\s+/g, " ");
      return v.length > 88 ? v.slice(0, 88) + "…" : v;
    }
  }
  const s = JSON.stringify(args);
  if (s === "{}" || s === "[]") return "";
  return s.length > 88 ? s.slice(0, 88) + "…" : s;
}

function unwrapUser(text) {
  const q = text.match(/<user_query>\s*([\s\S]*?)\s*<\/user_query>/);
  let body = (q ? q[1] : text).trim();
  const agent = body.match(
    /\[SAND_HIDDEN_PROMPT\]\[agent\] A message just arrived from another of your user's agents: (.+?) \(id: ([^)]+)\)[\s\S]*?\n\n\1: ([\s\S]*?)\n\nIf it needs a reply/
  );
  if (agent) {
    return { kind: "agent", name: agent[1], id: agent[2], text: agent[3].trim() };
  }
  body = body.replace(/\[SAND_HIDDEN_PROMPT\][\s\S]*?\[\/SAND_HIDDEN_PROMPT\]\s*/g, "");
  body = body.replace(/^\[SAND_HIDDEN_PROMPT\][\s\S]*?(?:\n\n|$)/, "");
  body = body.replace(/^\[Agents mentioned[\s\S]*?\]\s*/, "");
  body = body.replace(/^\[Skills invoked[\s\S]*?\]\s*/, "");
  return { kind: "user", text: body.trim() || text };
}


function fileAttrs(tok) {
  const out = {};
  String(tok).replace(/(\w+)="([^"]*)"/g, (_, k, v) => { out[k] = v; return ""; });
  return out;
}

function filePill(tok, pending) {
  const a = typeof tok === "string" ? fileAttrs(tok) : tok;
  const pill = el("a", "file-pill");
  pill.href = a.url || "#";
  if (a.url) { pill.target = "_blank"; pill.rel = "noopener noreferrer"; }
  else pill.addEventListener("click", (e) => e.preventDefault());
  if (a.mime && a.mime.indexOf("image/") === 0 && a.url) {
    const img = document.createElement("img");
    img.alt = "";
    img.src = a.url;
    pill.appendChild(img);
  } else {
    pill.appendChild(el("span", "att-ic", a.mime && a.mime.indexOf("image/") === 0 ? "🖼" : "📎"));
  }
  pill.appendChild(el("span", "n", a.name || a.path || "file"));
  if (pending) {
    const x = el("button", "att-x", "×");
    x.type = "button";
    x.onclick = pending;
    pill.appendChild(x);
  }
  return pill;
}

function attachUrl(saved) {
  if (saved.url) {
    const u = saved.url + (saved.url.indexOf("?") >= 0 ? "&" : "?") + "token=" + encodeURIComponent(TOKEN);
    return u;
  }
  return "";
}

function formatText(source) {
  return renderMarkdown(source, {
    fileNode(token) {
      const attachment = fileAttrs(token);
      if (attachment.path && attachment.path.startsWith("/workspace/tmp/")) {
        const parts = attachment.path.split("/");
        const uid = parts[3];
        const name = parts.slice(4).join("/");
        if (current && uid && name) {
          attachment.url = "/api/bots/" + encodeURIComponent(current) + "/attachments/" +
            encodeURIComponent(uid) + "/" + encodeURIComponent(name) +
            "?token=" + encodeURIComponent(TOKEN);
        }
      }
      return filePill(attachment);
    },
  });
}

function openStart(bot) {
  const speaker = (bot && bot.id) || (bot && bot.name) || "bot";
  const who = bot ? bot.name : "bot";
  const msg = el("div", "msg start");
  msg.dataset.speaker = speaker;
  const av = el("div", "msg-av");
  av.appendChild(avatar((bot && bot.id) || who, bot && bot.avatar, 28));
  const col = el("div", "msg-col");
  col.appendChild(el("div", "msg-head", who));
  msg.appendChild(av);
  msg.appendChild(col);
  track(msg, "msg");
  return col;
}

function addMessage(align, variant, who, text, avSpec, avId, attachments = []) {
  const body = text == null ? "" : String(text).trim();
  if (!body && !attachments.length) return null;
  const speaker = avId || who || "";
  const msg = el("div", "msg " + align);
  msg.dataset.speaker = speaker;
  msg.dataset.body = body;
  const av = el("div", "msg-av");
  av.appendChild(avatar(avId || who, avSpec, 28));
  const col = el("div", "msg-col");
  if (who && align === "start") col.appendChild(el("div", "msg-head", who));
  if (body) {
    const bubble = el("div", "bubble " + variant);
    bubble.appendChild(formatText(body));
    col.appendChild(bubble);
  }
  if (attachments.length) {
    const stack = el("div", "attachment-stack");
    for (const attachment of attachments) {
      const card = document.createElement("reve-attachment");
      card.data = { attachment, botId: current, token: TOKEN };
      stack.appendChild(card);
    }
    col.appendChild(stack);
  }
  msg.appendChild(av);
  msg.appendChild(col);
  track(msg, "msg");
  return msg;
}

function clipOut(out) {
  if (!out) return "";
  const s = String(out).replace(/\s+/g, " ").trim();
  if (!s) return "";
  return s.length > 240 ? s.slice(0, 240) + "…" : s;
}

function addSecretAsk(args, opt) {
  args = asArgs(args);
  const col = openStart(opt.bot);
  const card = el("div", "tool secret-card" + (opt.running ? " running" : "") + (opt.err ? " err" : ""));
  if (opt.id) card.dataset.id = opt.id;
  const row = el("div", "tool-row");
  row.appendChild(el("span", "tool-name", "AskUserForSecret"));
  row.appendChild(el("span", "tool-args", args.title || args.env || "secret"));
  card.appendChild(row);
  const live = !!opt.running && !opt.out;
  if (live) {
    const form = buildSecretForm(args, card);
    card.appendChild(form);
  } else if (opt.out) {
    card.appendChild(el("div", "tool-out", clipOut(opt.out)));
  }
  col.appendChild(card);
  if (!feed.rendering) scheduleFlush();
  return card;
}

function buildSecretForm(args, card) {
  const form = el("form", "secret-form");
  const title = args.title || "Secret";
  const why = [args.description, args.reason].filter(Boolean).join(" — ");
  form.appendChild(el("div", "secret-why", why || title));

  const envLab = el("label", "", "ENV_NAME");
  const env = el("input");
  env.type = "text";
  env.value = args.env || args.ENV_NAME || "";
  env.autocomplete = "off";
  env.spellcheck = false;
  form.appendChild(envLab);
  form.appendChild(env);

  const hostsLab = el("label", "", "Only attach to these HTTP hosts (comma-separated)");
  const hosts = el("input");
  hosts.type = "text";
  hosts.placeholder = "api.github.com, github.com";
  hosts.autocomplete = "off";
  form.appendChild(hostsLab);
  form.appendChild(hosts);

  const headLab = el("label", "", "Overwrite this request header with the secret");
  const header = el("input");
  header.type = "text";
  header.value = "Authorization";
  header.autocomplete = "off";
  form.appendChild(headLab);
  form.appendChild(header);

  const preLab = el("label", "", "Header prefix");
  const prefix = el("input");
  prefix.type = "text";
  prefix.value = "Bearer";
  prefix.placeholder = "Bearer, Token, or empty";
  prefix.autocomplete = "off";
  form.appendChild(preLab);
  form.appendChild(prefix);

  const kindBox = el("div", "secret-radios");
  kindBox.appendChild(el("label", "", "How to obtain it"));
  const kinds = [
    ["paste", "Paste the secret (password field)"],
    ["env", "Read a host environment variable"],
    ["command", "Run a shell script on the host"],
    ["http", "GET an HTTP URL on the host"]
  ];
  kinds.forEach(([val, label], i) => {
    const row = el("label");
    const r = el("input");
    r.type = "radio";
    r.name = "secret-kind-" + (card.dataset.id || "x");
    r.value = val;
    if (i === 0) r.checked = true;
    row.appendChild(r);
    row.appendChild(document.createTextNode(label));
    kindBox.appendChild(row);
  });
  form.appendChild(kindBox);

  const srcLab = el("label", "", "Value");
  const src = el("input");
  src.type = "password";
  src.placeholder = "paste secret";
  src.autocomplete = "off";
  form.appendChild(srcLab);
  form.appendChild(src);

  function kind() {
    const r = form.querySelector("input[type=radio]:checked");
    return r ? r.value : "paste";
  }
  function paintKind() {
    const k = kind();
    src.type = k === "paste" ? "password" : "text";
    src.placeholder = k === "paste" ? "paste secret"
      : k === "env" ? "HOST_ENV_NAME"
      : k === "command" ? "gh auth token"
      : "https://…";
    srcLab.textContent = k === "paste" ? "Secret"
      : k === "env" ? "Host env var"
      : k === "command" ? "Host command"
      : "HTTP URL";
  }
  form.querySelectorAll("input[type=radio]").forEach((r) => { r.onchange = paintKind; });
  paintKind();

  const actions = el("div", "secret-actions");
  const no = el("button", "no", "Decline");
  no.type = "button";
  const go = el("button", "go", "Save");
  go.type = "submit";
  actions.appendChild(no);
  actions.appendChild(go);
  form.appendChild(actions);

  async function submit(accept) {
    go.disabled = true;
    no.disabled = true;
    const k = kind();
    const body = {
      accept,
      env: env.value.trim(),
      kind: k,
      source: k === "paste" ? "" : src.value.trim(),
      value: k === "paste" ? src.value : "",
      hosts: hosts.value.split(/[,\s]+/).map((s) => s.trim()).filter(Boolean),
      header: header.value.trim() || null,
      prefix: prefix.value.trim() || null
    };
    try {
      await api("/api/bots/" + encodeURIComponent(current) + "/secrets", {
        method: "POST", body: JSON.stringify(body)
      });
      src.value = "";
    } catch (e) {
      go.disabled = false;
      no.disabled = false;
      feed.showStatus(e.message);
    }
  }
  no.onclick = () => submit(false);
  form.onsubmit = (ev) => { ev.preventDefault(); submit(true); };
  return form;
}

function addMarker(text) {
  const m = el("div", "marker", text);
  track(m, "marker");
  return m;
}

function groupOf(bot) {
  return (bot.group && bot.group.trim()) || "Unassigned";
}

async function loadBots() {
  ensureHouseEvents();
  const data = await api("/api/bots");
  bots = data.bots || [];
  if (current && !bots.some(bot => bot.id === current)) {
    logs.delete(current); logIds.delete(current);
    if (ws) { ws.onclose = null; ws.close(); }
    current = null; ++connection; rowCache.clear(); resetTranscript();
    composer.botId = "";
    feed.botId = "";
  }
  const q = (document.getElementById("q").value || "").toLowerCase();
  const box = document.getElementById("bots");
  box.textContent = "";
  const groups = {};
  for (const bot of bots) {
    if (q && !bot.name.toLowerCase().includes(q) && !bot.id.toLowerCase().includes(q)) continue;
    const g = groupOf(bot);
    (groups[g] = groups[g] || []).push(bot);
  }
  const names = Object.keys(groups).sort((a, b) => {
    if (a === "Unassigned") return 1;
    if (b === "Unassigned") return -1;
    return a.localeCompare(b);
  });
  const onlyDefault = names.length === 1 && names[0] === "Unassigned";
  for (const g of names) {
    if (!onlyDefault) box.appendChild(el("div", "group-label", g));
    for (const bot of groups[g]) {
      const d = el("div", "bot" + (current === bot.id ? " on" : ""));
      d.dataset.id = bot.id;
      if (bot.busy) busy[bot.id] = true;
      else delete busy[bot.id];
      d.appendChild(avatar(bot.id, bot.avatar, 42));
      const meta = el("div", "meta");
      const l1 = el("div", "l1");
      l1.appendChild(el("span", "name", bot.name));
      const badge = modelBadge(bot.model);
      if (badge) l1.appendChild(el("span", "badge", badge));
      meta.appendChild(l1);
      meta.appendChild(el("div", "prev", bot.profile_error ? "Profile error: " + bot.profile_error : (bot.title || bot.description || bot.id)));
      d.appendChild(meta);
      d.onclick = () => { select(bot.id); closeDrawers(); };
      d.oncontextmenu = (e) => { e.preventDefault(); openCtx(e, bot); };
      box.appendChild(d);
    }
  }
  if (!current && bots.length) {
    const want = routeBot();
    const hit = want && bots.find((b) => b.id === want);
    if (hit) select(hit.id, true);
    else {
      select(bots[0].id, true);
      setRoute(bots[0].id);
    }
  }
  if (current) paintHead(bots.find((bot) => bot.id === current));
  paintBusy();
  if (current) queueLogRender(current);
}

function paintHead(bot) {
  const av = document.getElementById("head-av");
  av.innerHTML = "";
  if (bot) av.appendChild(avatar(bot.id, bot.avatar, 36));
  document.getElementById("head-name").textContent = bot ? bot.name : "revebot";
  document.getElementById("head-sub").textContent = bot ? (bot.profile_error ? "Profile error: " + bot.profile_error : (bot.title || bot.description || "")) : "";
  document.getElementById("screen-title").textContent = bot ? bot.name + "'s screen" : "Screen";
}

// Browser adapter. Every transcript update is log -> projection -> renderer.
const logs = new Map();
const logIds = new Map();
const rowCache = new Map();
const activityOpen = new Set();
const outputOpen = new Set();
const runningBots = new Set();
const disconnected = new Set();
const faults = new Map();
let connection = 0;
let streamReady = Promise.resolve();
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
    if (cached && cached.signature === signature) { feed.restore(cached.items); continue; }
    const start = feed.itemCount;
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
      node = addMessage('start', 'muted', bot ? bot.name : 'bot', row.text, bot && bot.avatar, bot && bot.id, row.attachments || []);
    } else addMarker(row.text);
    if (node) node.dataset.entryId = row.id;
    if (node && (row.label || row.status === 'cancelled')) {
      node.querySelector('.msg-col').appendChild(el('div', 'draft-label', row.label || 'Cancelled before being acted on'));
    }
    rowCache.set(row.id, {signature, items:feed.itemsFrom(start)});
  }
}

function renderStatus(id) {
  if (current !== id) return;
  if (faults.has(id)) {
    setBusy(id, false);
    feed.showStatus('Session error: ' + faults.get(id));
    return;
  }
  if (disconnected.has(id)) {
    setBusy(id, false);
    feed.showStatus('Disconnected — reconnecting…');
    return;
  }
  const rows = ReveLog.project(chatLog(id).records());
  const active = rows.filter(r => (r.kind === 'tool' || r.kind === 'secret') && r.running).at(-1);
  const working = runningBots.has(id) || !!active || chatLog(id).records().some(r => r.status === 'streaming');
  setBusy(id, working);
  feed.showStatus(working ? el('span', 'shimmer', 'Working…' + (active ? ' ' + ReveLog.action(active) : '')) : '');
}

function rebuild(bot, keepScroll) {
  if (!current) return;
  const snapshot = feed.beginRender();
  const rows = ReveLog.group(ReveLog.project(chatLog(current).records()));
  const visibleIds = new Set(rows.map(row => row.id));
  for (const id of rowCache.keys()) if (!visibleIds.has(id)) rowCache.delete(id);
  paintLog(bot, rows);
  feed.finishRender(snapshot, keepScroll);
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
  const data = await api(
    '/api/bots/' + encodeURIComponent(id) + '/messages?limit=' + PAGE,
    {},
    { topic: 'conversation-snapshot', owner: { botId: id, conversationId: expectedId } },
  );
  if (!valid()) return;
  if (!Array.isArray(data.records) || typeof data.log_id !== 'string') throw new Error('Invalid log snapshot');
  if (data.log_id !== expectedId) throw new Error('Bot log changed; reconnecting');
  if (logIds.has(id) && logIds.get(id) !== data.log_id) {
    logs.delete(id); rowCache.clear();
    if (current === id) composer.value = "";
  }
  logIds.set(id, data.log_id);
  const log = chatLog(id);
  const received = new Set((data.records || []).map(r => r.entry.id));
  const outstanding = log.records().filter(r => (r.status === 'streaming' || r.status === 'accepted') && !received.has(r.entry.id));
  log.merge(data.records || []);
  for (const record of outstanding) {
    try {
      log.upsert(await api(
        '/api/bots/' + encodeURIComponent(id) + '/messages/' + encodeURIComponent(record.entry.id),
        {},
        { topic: 'conversation-record', owner: { botId: id, conversationId: expectedId } },
      ));
    }
    catch (error) {
      if (error.status !== 404) throw error;
      log.upsert({...record, revision:0, status:record.status === 'accepted' ? 'cancelled' : 'interrupted'});
    }
  }
  if (!valid()) return;
  if (data.operation_id) runningBots.add(id); else runningBots.delete(id);
  if (current === id) {
    const committed = chatLog(id).records().filter(r => r.status === 'committed');
    feed.setHistory({
      hasMore: data.has_more,
      oldestSeq: committed.length ? committed[0].order : null,
    });
    queueLogRender(id);
  }
}

async function loadOlder() {
  const history = feed.history;
  if (!current || !history.hasMore || feed.loadingOlder || !history.oldestSeq) return;
  feed.loadingOlder = true;
  const id = current, epoch = connection, expectedId = logIds.get(current);
  try {
    const data = await api(
      '/api/bots/' + encodeURIComponent(id) + '/messages?limit=' + PAGE + '&before=' + history.oldestSeq,
      {},
      { topic: 'conversation-page', owner: { botId: id, conversationId: expectedId } },
    );
    if (current !== id || connection !== epoch || data.log_id !== expectedId) return;
    chatLog(id).merge(data.records || []);
    if (current === id && connection === epoch) {
      feed.setHistory({ hasMore: data.has_more, oldestSeq: data.oldest_seq || history.oldestSeq });
      rebuild(bots.find(b => b.id === id), true);
    }
  } finally {
    if (connection === epoch) feed.loadingOlder = false;
  }
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
        disconnected.delete(id);
        const events = buffered; buffered = [];
        for (const event of events) apply(event);
        resolve();
      } catch (error) { reject(error); socket.close(); }
      finally { syncing = false; }
    }
    socket.onopen = () => {}; // wait for the subscribed session's identity
    socket.onmessage = message => {
      const event = JSON.parse(message.data);
      const isCurrent = valid();
      emitServerState({
        source: "websocket", requestId: null, topic: "bot-event",
        owner: { botId: id, conversationId: event.log_id || identity || null },
        channel: "bot", ok: true, data: event, stale: !isCurrent,
      });
      if (!isCurrent) return;
      if (event.type === 'hello') { identity = event.log_id; sync(); return; }
      if (event.type === 'lagged') { sync(); return; }
      if (!ready) buffered.push(event); else apply(event);
    };
    socket.onerror = () => { if (!ready) reject(new Error('Could not connect to the bot log')); };
    socket.onclose = () => {
      if (!valid()) return;
      emitServerState({
        source: "websocket", requestId: null, topic: "bot-connection",
        owner: { botId: id, conversationId: identity },
        channel: "bot", ok: false, status: 0, error: "Disconnected — reconnecting…",
      });
      if (!ready) reject(new Error('Bot log connection closed'));
      chatLog(id).interrupt(); runningBots.delete(id); disconnected.add(id); queueLogRender(id);
      const profile = bots.find(bot => bot.id === id);
      if (!profile || profile.status !== 'ready') return;
      setTimeout(() => {
        if (valid()) streamReady = connectLog(id, epoch).catch(error => { if (current === id) feed.showStatus(error.message); });
      }, 1000);
    };
  });
}

async function reconnectConversation(id) {
  if (ws) { ws.onclose = null; ws.close(); ws = null; }
  const epoch = ++connection;
  logs.delete(id);
  logIds.delete(id);
  rowCache.clear();
  feed.loadingOlder = false;
  resetTranscript();
  streamReady = connectLog(id, epoch);
  try {
    await streamReady;
  } catch (error) {
    if (current === id) feed.showStatus(error.message);
    throw error;
  }
}

function freezeConversation(botId, conversationId) {
  if (current !== botId) return;
  connection += 1;
  if (ws) { ws.onclose = null; ws.close(); ws = null; }
  frozenConversation = { botId, conversationId };
  composer.readOnly = true;
  feed.showStatus("Creating fork · this tab will remain on the source conversation");
}

function openForkTab() {
  const tab = window.open("about:blank", "_blank");
  if (tab) {
    tab.opener = null;
    tab.document.title = "Opening fork…";
    tab.document.body.textContent = "Opening fork…";
  }
  return tab;
}

function activeConversationUrl(botId) {
  return location.origin + location.pathname + location.search + "#/" + encodeURIComponent(botId);
}

function parseConversationCommand(text, attachmentCount) {
  if (attachmentCount) return null;
  const match = /^\/(compact|new|fork)(?:\s+([\s\S]*))?$/.exec((text || "").trim());
  if (!match) return null;
  return {
    command: match[1],
    instructions: match[1] === "compact" ? (match[2] || "").trim() || null : null,
    invalidArguments: match[1] !== "compact" && Boolean((match[2] || "").trim()),
  };
}

async function runConversationCommand({
  botId,
  command,
  instructions = null,
  entryId = null,
  requestId = null,
  topic = "conversation-command",
  forkTab = null,
}) {
  let logId = logIds.get(botId) || bots.find((bot) => bot.id === botId)?.log_id;
  if (!logId) {
    await streamReady;
    logId = logIds.get(botId);
  }
  if (!logId) throw new Error("Connect to the bot log before running a command");
  const frozeSource = command === "fork" && Boolean(forkTab) && current === botId;
  if (frozeSource) freezeConversation(botId, logId);
  let data;
  try {
    data = await api(
      "/api/bots/" + encodeURIComponent(botId) + "/commands",
      {
        method: "POST",
        body: JSON.stringify({
          command,
          log_id: logId,
          instructions,
          entry_id: entryId,
        }),
      },
      { requestId, topic, owner: { botId, conversationId: logId } },
    );
  } catch (error) {
    if (frozeSource && current === botId) {
      frozenConversation = null;
      composer.readOnly = false;
      await reconnectConversation(botId).catch(() => {});
    }
    throw error;
  }
  if (command === "compact") return data;
  await loadBots().catch(console.error);
  if (command === "new") {
    if (current === botId) {
      frozenConversation = null;
      composer.readOnly = false;
      await reconnectConversation(botId).catch(() => {});
    }
    return data;
  }
  if (forkTab) {
    if (frozeSource) feed.showStatus("Fork created · this tab remains on the source conversation");
    forkTab.location.href = activeConversationUrl(botId);
  } else if (current === botId) {
    frozenConversation = null;
    composer.readOnly = false;
    await reconnectConversation(botId).catch(() => {});
  }
  return data;
}

async function select(id, fromRoute) {
  if (!id) return;
  if (current === id) {
    if (frozenConversation) {
      frozenConversation = null;
      composer.readOnly = false;
      await reconnectConversation(id);
    }
    if (!fromRoute) setRoute(id);
    closeDrawers();
    return;
  }
  frozenConversation = null;
  composer.readOnly = false;
  current = id; const epoch = ++connection;
  composer.botId = id;
  composer.setStatusline(statuslines[id] || "");
  feed.botId = id;
  if (!fromRoute) setRoute(id);
  if (ws) { ws.onclose = null; ws.close(); }
  rowCache.clear(); feed.loadingOlder = false; resetTranscript();
  const paint = () => {
    paintHead(bots.find(b => b.id === id));
    queueLogRender(id);
    closeDrawers();
  };
  if (document.startViewTransition) document.startViewTransition(paint); else paint();
  const profile = bots.find(bot => bot.id === id);
  if (profile && profile.status !== 'ready') {
    feed.showStatus(profile.profile_error || 'Bot is unavailable');
    return;
  }
  streamReady = connectLog(id, epoch);
  loadBots().catch(console.error);
  loadRoutines().catch(console.error);
  try { await streamReady; }
  catch (error) { if (current === id) feed.showStatus(error.message); }
}

function routeBot() {
  const raw = (location.hash || "").replace(/^#\/?/, "");
  if (!raw) return "";
  const id = raw.split("/")[0];
  try { return decodeURIComponent(id); } catch (_) { return id; }
}

function setRoute(id) {
  if (!id) return;
  const next = "#/" + encodeURIComponent(id);
  if (location.hash === next) return;
  history.replaceState(null, "", next);
}

async function loadRoutines() {
  const data = await api("/api/routines");
  const box = document.getElementById("routines");
  box.textContent = "";
  const mine = (data.routines || []).filter((r) => !r.bot || r.bot === current);
  const house = (data.routines || []).filter((r) => r.bot && r.bot !== current);
  const list = mine.concat(house);
  for (const r of list) {
    const d = el("div", "routine" + (r.enabled ? "" : " off"));
    d.appendChild(el("div", "rn", r.name));
    const who = r.bot && r.bot !== current ? r.bot + " · " : "";
    d.appendChild(el("div", "rs", who + r.schedule + (r.enabled ? "" : " · off")));
    d.title = r.enabled ? "Run now" : "Disabled — edit the routine Lua";
    if (r.enabled) d.onclick = async () => {
      feed.showStatus("Working… " + r.name);
      await api("/api/routines/" + encodeURIComponent(r.id) + "/run", { method: "POST", body: "{}" });
    };
    box.appendChild(d);
  }
  if (!box.childNodes.length) {
    box.appendChild(el("div", "rs", "No routines. Add Lua under workspace/agents/<id>/routines/."));
  }
}

function joinPath(a, b) {
  if (!a) return b;
  return a.replace(/\/+$/, "") + "/" + b;
}

const TREE_ICO = {
  chevron: '<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M6.7 4.3a.75.75 0 0 0 0 1.06L9.34 8 6.7 10.64a.75.75 0 1 0 1.06 1.06l3.17-3.17a.75.75 0 0 0 0-1.06L7.76 4.3a.75.75 0 0 0-1.06 0z"/></svg>',
  folder: '<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="#dcb67a" d="M1.5 3.25c0-.69.56-1.25 1.25-1.25h3.17c.27 0 .53.09.74.25L8.1 3.5h6.15c.69 0 1.25.56 1.25 1.25v7.5c0 .69-.56 1.25-1.25 1.25h-11.5c-.69 0-1.25-.56-1.25-1.25z"/></svg>',
  folderOpen: '<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="#dcb67a" d="M1.5 3.25c0-.69.56-1.25 1.25-1.25h3.17c.27 0 .53.09.74.25L8.1 3.5h.65l-1.12 5.5H2.4L1.5 4.6zm.9 6.75h5.72c.3 0 .57-.2.66-.48l1.55-5.02h3.42c.69 0 1.25.56 1.25 1.25v7.5c0 .69-.56 1.25-1.25 1.25H2.75c-.69 0-1.25-.56-1.25-1.25V10z"/></svg>',
  file: '<svg viewBox="0 0 16 16" aria-hidden="true"><path fill="#8b8b8b" d="M4 1.5h5.2L13.5 6v8.5H4z"/><path fill="#1e1e1e" d="M9.2 1.5V6h4.3z"/></svg>'
};

let treeGen = 0;
let treeType = "";
let treeTypeT = 0;
let treeReady = false;
let treeLoad = null;

function treeDepth(row) {
  return Number(row.style.getPropertyValue("--d")) || 0;
}

function treeKids(row) {
  const n = row.nextElementSibling;
  return n && n.classList.contains("tree-kids") ? n : null;
}

function parentRow(row) {
  const kids = row.parentElement && row.parentElement.parentElement;
  if (!kids || !kids.classList.contains("tree-kids")) return null;
  return kids.previousElementSibling;
}

function visibleRows() {
  return [...document.querySelectorAll("#tree .tree-row")].filter((r) => r.offsetParent);
}

function selectRow(row, scroll) {
  if (!row) return;
  document.querySelectorAll("#tree .tree-row.on").forEach((n) => {
    n.classList.remove("on");
    n.removeAttribute("aria-selected");
  });
  row.classList.add("on");
  row.setAttribute("aria-selected", "true");
  if (scroll) row.scrollIntoView({ block: "nearest" });
}

function setFolderIcon(row, open) {
  const ico = row.querySelector(".tree-ico");
  if (ico) ico.innerHTML = open ? TREE_ICO.folderOpen : TREE_ICO.folder;
}

async function expandRow(row) {
  if (!row.classList.contains("dir") || row.classList.contains("open")) return;
  row.classList.add("open");
  row.setAttribute("aria-expanded", "true");
  setFolderIcon(row, true);
  const kids = treeKids(row);
  if (!kids) return;
  kids.hidden = false;
  if (row.dataset.loaded || row.dataset.loading) return;
  row.dataset.loading = "1";
  try {
    kids.appendChild(await treeBranch(row.dataset.path || "", treeDepth(row) + 1));
    row.dataset.loaded = "1";
  } catch (err) {
    kids.appendChild(el("div", "rs", err.message || "unreadable"));
  } finally {
    delete row.dataset.loading;
  }
}

function collapseRow(row) {
  if (!row.classList.contains("dir") || !row.classList.contains("open")) return;
  row.classList.remove("open");
  row.setAttribute("aria-expanded", "false");
  setFolderIcon(row, false);
  const kids = treeKids(row);
  if (kids) kids.hidden = true;
}

function collapseTree() {
  document.querySelectorAll("#tree .tree-row.dir.open").forEach((row) => {
    if (row.dataset.path) collapseRow(row);
  });
}

async function expandPath(path) {
  if (!path) return;
  let acc = "";
  for (const part of path.split("/")) {
    acc = acc ? acc + "/" + part : part;
    const row = document.querySelector("#tree .tree-row[data-path=\"" + CSS.escape(acc) + "\"]");
    if (!row || !row.classList.contains("dir")) return;
    await expandRow(row);
  }
}

function openPaths() {
  return [...document.querySelectorAll("#tree .tree-row.dir.open")]
    .map((r) => r.dataset.path)
    .filter((p) => p);
}

function treeNode(path, name, isDir, depth) {
  const node = el("div", "tree-node");
  const row = el("div", "tree-row" + (isDir ? " dir" : ""));
  row.style.setProperty("--d", String(depth));
  row.dataset.path = path;
  row.title = path ? "/workspace/" + path : "/workspace";
  row.setAttribute("role", "treeitem");
  row.setAttribute("aria-level", String(depth + 1));
  if (isDir) row.setAttribute("aria-expanded", "false");
  for (let i = 0; i < depth; i++) {
    const g = el("span", "tree-guide");
    g.style.left = (4 + i * 16 + 8) + "px";
    row.appendChild(g);
  }
  const tw = el("span", "tree-tw");
  tw.innerHTML = TREE_ICO.chevron;
  const ico = el("span", "tree-ico");
  ico.innerHTML = isDir ? TREE_ICO.folder : TREE_ICO.file;
  row.appendChild(tw);
  row.appendChild(ico);
  row.appendChild(el("span", "tree-name", name));
  if (isDir) {
    tw.onclick = async (e) => {
      e.stopPropagation();
      document.getElementById("tree").focus({ preventScroll: true });
      selectRow(row);
      if (row.classList.contains("open")) collapseRow(row);
      else await expandRow(row);
    };
  }
  row.onclick = async (e) => {
    e.stopPropagation();
    document.getElementById("tree").focus({ preventScroll: true });
    selectRow(row);
    if (isDir) {
      if (!row.classList.contains("open")) await expandRow(row);
    } else {
      previewFile(path);
    }
  };
  node.appendChild(row);
  if (isDir) {
    const kids = el("div", "tree-kids");
    kids.hidden = true;
    kids.setAttribute("role", "group");
    node.appendChild(kids);
  }
  return node;
}

async function treeBranch(path, depth) {
  const frag = document.createDocumentFragment();
  const data = await api("/api/fs?path=" + encodeURIComponent(path));
  for (const ent of data.entries || []) {
    frag.appendChild(treeNode(joinPath(path, ent.name), ent.name, !!ent.dir, depth));
  }
  return frag;
}

async function ensureTree() {
  if (treeReady && document.querySelector("#tree .tree-row")) return;
  if (treeLoad) return treeLoad;
  treeLoad = (async () => {
    try {
      await loadTree();
      treeReady = true;
    } catch (e) {
      treeReady = false;
      throw e;
    } finally {
      treeLoad = null;
    }
  })();
  return treeLoad;
}

async function loadTree() {
  const gen = ++treeGen;
  const box = document.getElementById("tree");
  const keep = openPaths();
  const on = (box.querySelector(".tree-row.on") || {}).dataset;
  const sel = on && on.path;
  box.textContent = "";
  try {
    const root = treeNode("", "workspace", true, 0);
    box.appendChild(root);
    await expandRow(root.querySelector(".tree-row"));
    if (gen !== treeGen) return;
    for (const p of keep) {
      await expandPath(p);
      if (gen !== treeGen) return;
    }
    if (sel) {
      const row = box.querySelector(".tree-row[data-path=\"" + CSS.escape(sel) + "\"]");
      if (row) selectRow(row, true);
    }
  } catch (e) {
    if (gen !== treeGen) return;
    box.appendChild(el("div", "rs", e.message || "unreadable"));
  }
}

function treeTypefind(ch) {
  treeType += ch.toLowerCase();
  clearTimeout(treeTypeT);
  treeTypeT = setTimeout(() => { treeType = ""; }, 600);
  const rows = visibleRows();
  if (!rows.length) return;
  const i = rows.findIndex((r) => r.classList.contains("on"));
  const ordered = rows.slice(i + 1).concat(rows.slice(0, i + 1));
  const hit = ordered.find((r) => {
    const n = r.querySelector(".tree-name");
    return n && n.textContent.toLowerCase().startsWith(treeType);
  });
  if (hit) selectRow(hit, true);
}

function treeKey(e) {
  const rows = visibleRows();
  if (!rows.length) return;
  let i = rows.findIndex((r) => r.classList.contains("on"));
  if (i < 0) i = 0;
  const row = rows[i];
  if (e.key === "ArrowDown") {
    e.preventDefault();
    selectRow(rows[Math.min(i + 1, rows.length - 1)], true);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    selectRow(rows[Math.max(i - 1, 0)], true);
  } else if (e.key === "ArrowRight") {
    e.preventDefault();
    if (row.classList.contains("dir")) {
      if (!row.classList.contains("open")) expandRow(row);
      else if (rows[i + 1]) selectRow(rows[i + 1], true);
    }
  } else if (e.key === "ArrowLeft") {
    e.preventDefault();
    if (row.classList.contains("dir") && row.classList.contains("open")) collapseRow(row);
    else {
      const p = parentRow(row);
      if (p) selectRow(p, true);
    }
  } else if (e.key === "Enter") {
    e.preventDefault();
    row.click();
  } else if (e.key === "Home") {
    e.preventDefault();
    selectRow(rows[0], true);
  } else if (e.key === "End") {
    e.preventDefault();
    selectRow(rows[rows.length - 1], true);
  } else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
    treeTypefind(e.key);
  }
}

async function previewFile(path) {
  const body = document.querySelector(".screen .body");
  const title = document.getElementById("screen-title");
  const peek = document.getElementById("file-peek");
  const peekBody = document.getElementById("file-peek-body");
  const peekName = document.getElementById("file-peek-name");
  const name = path.split("/").pop() || path || "workspace";
  body.classList.add("file");
  title.textContent = name;
  peek.hidden = false;
  peekName.textContent = path ? "/workspace/" + path : "/workspace";
  try {
    const data = await api("/api/fs/file?path=" + encodeURIComponent(path));
    const text = data.binary
      ? path + "\n\nbinary file"
      : (data.text || "") + (data.truncated ? "\n\n… truncated" : "");
    body.textContent = text;
    peekBody.textContent = text;
  } catch (e) {
    const msg = e.message || "unreadable";
    body.textContent = msg;
    peekBody.textContent = msg;
  }
}

function showSide(name) {
  document.querySelectorAll(".side-tab").forEach((t) => {
    const on = t.dataset.pane === name;
    t.classList.toggle("on", on);
    t.setAttribute("aria-selected", on ? "true" : "false");
  });
  document.querySelectorAll(".side-pane").forEach((p) => {
    p.classList.toggle("on", p.id === "pane-" + name);
  });
  if (name === "files") ensureTree().catch(() => {});
}

const fsStat = new Map();

function pathInfo(rel) {
  const key = rel || "";
  if (fsStat.has(key)) return fsStat.get(key);
  const p = api("/api/fs/stat?path=" + encodeURIComponent(key))
    .then((d) => ({ exists: !!d.exists, dir: !!d.dir }))
    .catch(() => ({ exists: false, dir: false }));
  fsStat.set(key, p);
  return p;
}

function asWorkspacePath(raw) {
  if (raw == null) return null;
  let s = String(raw).trim();
  s = s.replace(/^[`'"]+|['"`]+$/g, "");
  s = s.replace(/[.,;:!?)]+$/g, "");
  if (!s || /\s/.test(s) || s.includes("://")) return null;
  if (s === "/workspace" || s === "/workspace/") return "";
  if (s.startsWith("/workspace/")) s = s.slice("/workspace/".length);
  else if (s.startsWith("/")) return null;
  else s = s.replace(/^\.\//, "");
  s = s.replace(/\/+$/, "");
  if (!s) return "";
  if (s.split("/").some((p) => !p || p === "." || p === "..")) return null;
  if (!s.includes("/") && !/\.(md|txt|json|ya?ml|lua|rs|toml|html|css|js|ts|py|sh|svg|png|jpe?g|webp|lock|jsonl|mdc)$/i.test(s)) {
    return null;
  }
  return s;
}

function pathMatchAt(text, offset) {
  const re = /\/workspace(?:\/[\w.+\-]+)*\/?/g;
  let m;
  while ((m = re.exec(text))) {
    if (offset >= m.index && offset <= m.index + m[0].length) return m;
  }
  return null;
}

function fileTokenAt(text, offset) {
  const abs = pathMatchAt(text, offset);
  if (abs) return abs;
  let s = offset, e = offset;
  while (s > 0 && /[\w./+\-]/.test(text[s - 1])) s--;
  while (e < text.length && /[\w./+\-]/.test(text[e])) e++;
  const tok = text.slice(s, e);
  if (asWorkspacePath(tok) == null) return null;
  return { 0: tok, index: s };
}

function caretFromPoint(x, y) {
  if (document.caretRangeFromPoint) {
    const r = document.caretRangeFromPoint(x, y);
    if (!r) return null;
    return { node: r.startContainer, offset: r.startOffset };
  }
  if (document.caretPositionFromPoint) {
    const p = document.caretPositionFromPoint(x, y);
    if (!p) return null;
    return { node: p.offsetNode, offset: p.offset };
  }
  return null;
}

let liveRef = null;

function unwrapFileRef(ref) {
  if (!ref) return;
  if (liveRef === ref) liveRef = null;
  if (!ref.isConnected) return;
  if (ref.tagName === "CODE") {
    ref.classList.remove("file-ref");
    delete ref.dataset.path;
    ref.removeAttribute("title");
    return;
  }
  const parent = ref.parentNode;
  if (!parent) return;
  parent.replaceChild(document.createTextNode(ref.textContent), ref);
  parent.normalize();
}

function wrapTextRange(node, start, end, rel) {
  if (!node || !node.parentNode) return null;
  if (node.parentElement && node.parentElement.closest(".file-ref")) return null;
  const text = node.data;
  if (start < 0 || end > text.length || start >= end) return null;
  const span = el("span", "file-ref");
  span.dataset.path = rel;
  span.title = rel ? "/workspace/" + rel : "/workspace";
  span.textContent = text.slice(start, end);
  const after = document.createTextNode(text.slice(end));
  node.data = text.slice(0, start);
  node.parentNode.insertBefore(span, node.nextSibling);
  node.parentNode.insertBefore(after, span.nextSibling);
  if (liveRef && liveRef !== span) unwrapFileRef(liveRef);
  liveRef = span;
  return span;
}

function maybeLinkCode(code) {
  if (!code || code.classList.contains("file-ref")) return;
  if (code.closest(".fence")) return;
  const rel = asWorkspacePath(code.textContent);
  if (rel == null) return;
  pathInfo(rel).then((info) => {
    if (!info.exists || !code.isConnected) return;
    if (liveRef && liveRef !== code) unwrapFileRef(liveRef);
    code.classList.add("file-ref");
    code.dataset.path = rel;
    code.title = rel ? "/workspace/" + rel : "/workspace";
    liveRef = code;
  });
}

function inspectHover(x, y) {
  const hit = document.elementFromPoint(x, y);
  if (!hit || !hit.closest || !hit.closest("#log")) {
    unwrapFileRef(liveRef);
    return;
  }
  const existing = hit.closest(".file-ref");
  if (existing) {
    if (liveRef && liveRef !== existing) unwrapFileRef(liveRef);
    liveRef = existing;
    return;
  }
  unwrapFileRef(liveRef);
  if (hit.closest("a, textarea, input, .secret-form")) return;
  const code = hit.closest("code");
  if (code && !code.closest(".fence")) {
    maybeLinkCode(code);
    return;
  }
  const caret = caretFromPoint(x, y);
  if (!caret || caret.node.nodeType !== 3) return;
  if (caret.node.parentElement && caret.node.parentElement.closest("a")) return;
  const text = caret.node.data || "";
  const inFence = !!(caret.node.parentElement && caret.node.parentElement.closest(".fence"));
  const m = inFence ? pathMatchAt(text, caret.offset) : fileTokenAt(text, caret.offset);
  if (!m) return;
  const rel = asWorkspacePath(m[0]);
  if (rel == null) return;
  const start = m.index, end = m.index + m[0].length;
  const snap = text.slice(start, end);
  pathInfo(rel).then((info) => {
    if (!info.exists || !caret.node.isConnected) return;
    if (caret.node.data.slice(start, end) !== snap) return;
    wrapTextRange(caret.node, start, end, rel);
  });
}

async function revealFile(path) {
  const rel = asWorkspacePath(path);
  if (rel == null && path !== "" && path !== "/workspace") return;
  const guest = rel == null ? "" : rel;
  showSide("files");
  await ensureTree();
  const parts = guest.split("/").filter(Boolean);
  if (parts.length) await expandPath(parts.slice(0, -1).join("/"));
  const row = document.querySelector("#tree .tree-row[data-path=\"" + CSS.escape(guest) + "\"]");
  if (row) selectRow(row, true);
  const info = await pathInfo(guest);
  if (info.exists && !info.dir) previewFile(guest);
}

let sheetMode = "new";
let sheetBotId = null;
let sheetColor = "orange";
let sheetShape = "goutte";

function avatarSpec() { return sheetColor + ":" + sheetShape; }

function paintSheetAv() {
  const box = document.getElementById("sheet-av");
  box.innerHTML = "";
  const name = document.getElementById("f-name").value || "bot";
  box.appendChild(avatar(name, avatarSpec(), 72));
}

function paintPickers() {
  const sw = document.getElementById("swatches");
  sw.textContent = "";
  Object.keys(BLOUB.COLORS).forEach((c) => {
    const b = el("button", "swatch" + (c === sheetColor ? " on" : ""));
    b.type = "button";
    b.style.background = BLOUB.COLORS[c];
    b.title = c;
    b.onclick = () => { sheetColor = c; paintPickers(); paintSheetAv(); };
    sw.appendChild(b);
  });
  const sh = document.getElementById("shapes");
  sh.textContent = "";
  BLOUB.SHAPE_IDS.forEach((s) => {
    const b = el("button", "shape" + (s === sheetShape ? " on" : ""));
    b.type = "button";
    b.textContent = s.slice(0, 3);
    b.title = s;
    b.onclick = () => { sheetShape = s; paintPickers(); paintSheetAv(); };
    sh.appendChild(b);
  });
}

async function fillModels(selected) {
  const sel = document.getElementById("f-model");
  sel.textContent = "";
  sel.appendChild(Object.assign(el("option"), { value: "", textContent: "House default" }));
  let ids = [];
  try {
    const data = await api("/api/models");
    ids = data.models || [];
  } catch (e) {
    feed.showStatus(e.message || "could not load models.yml");
  }
  if (selected && !ids.includes(selected)) ids = [selected].concat(ids);
  const groups = {};
  ids.forEach((id) => {
    const i = id.indexOf("/");
    const g = i > 0 ? id.slice(0, i) : "other";
    (groups[g] = groups[g] || []).push(id);
  });
  Object.keys(groups).forEach((g) => {
    const og = document.createElement("optgroup");
    og.label = g;
    groups[g].forEach((id) => {
      const o = el("option");
      o.value = id;
      o.textContent = id;
      if (id === selected) o.selected = true;
      og.appendChild(o);
    });
    sel.appendChild(og);
  });
}

async function openSheet(mode, bot) {
  sheetMode = mode;
  sheetBotId = bot && bot.id || null;
  document.getElementById("sheet-title").textContent = mode === "new" ? "New bot" : "Edit " + (bot && bot.name || "bot");
  document.getElementById("sheet-save").textContent = mode === "new" ? "Create" : "Save";
  const parsed = BLOUB.parseSpec(bot && bot.avatar, bot && bot.id);
  sheetColor = parsed.color;
  sheetShape = parsed.shape;
  document.getElementById("f-name").value = bot ? bot.name : "";
  document.getElementById("f-title").value = bot ? (bot.title || "") : "";
  document.getElementById("f-desc").value = bot ? (bot.description || "") : "";
  document.getElementById("f-soul").value = "";
  await fillModels(bot && bot.model);
  if (mode === "edit" && bot) {
    try {
      const data = await api("/api/bots/" + encodeURIComponent(bot.id) + "/soul");
      document.getElementById("f-soul").value = data.text || "";
    } catch (_) {}
  }
  paintPickers();
  paintSheetAv();
  const sheet = document.getElementById("sheet");
  try { sheet.showPopover(); } catch (_) { sheet.classList.add("on"); }
  document.getElementById("f-name").focus();
}

function closeSheet() {
  const sheet = document.getElementById("sheet");
  try { sheet.hidePopover(); } catch (_) { sheet.classList.remove("on"); }
}

function createAgent() { openSheet("new", null); }

let ctxBot = null;

function positionContextMenu(menu, event) {
  const pad = 8;
  let x = event.clientX, y = event.clientY;
  menu.style.left = "0px";
  menu.style.top = "0px";
  const width = menu.offsetWidth, height = menu.offsetHeight;
  if (x + width > window.innerWidth - pad) x = window.innerWidth - width - pad;
  if (y + height > window.innerHeight - pad) y = window.innerHeight - height - pad;
  menu.style.left = Math.max(pad, x) + "px";
  menu.style.top = Math.max(pad, y) + "px";
}

function openCtx(e, bot) {
  ctxBot = bot;
  const menu = document.getElementById("ctx");
  menu.hidden = false;
  menu.classList.add("on");
  positionContextMenu(menu, e);
}

function closeCtx() {
  const menu = document.getElementById("ctx");
  menu.classList.remove("on");
  menu.hidden = true;
  ctxBot = null;
}

document.getElementById("ctx-edit").onclick = () => {
  const bot = ctxBot;
  closeCtx();
  if (bot) openSheet("edit", bot);
};
document.getElementById("ctx-delete").onclick = async () => {
  const bot = ctxBot;
  closeCtx();
  if (!bot) return;
  if (!confirm("Delete " + bot.name + "? This cannot be undone.")) return;
  await api("/api/bots/" + encodeURIComponent(bot.id), {
    method: "DELETE", body: JSON.stringify({ confirm: true })
  });
  if (current === bot.id) { current = null; }
  await loadBots();
};

let ctxMessage = null;

function closeMessageCtx() {
  const menu = document.getElementById("msg-ctx");
  menu.classList.remove("on");
  menu.hidden = true;
  ctxMessage = null;
}

feed.addEventListener("contextmenu", (event) => {
  const message = event.target.closest(".msg[data-entry-id]");
  if (!message) return;
  event.preventDefault();
  closeCtx();
  ctxMessage = {
    botId: current,
    entryId: message.dataset.entryId,
    text: message.dataset.body || "",
  };
  const menu = document.getElementById("msg-ctx");
  menu.hidden = false;
  menu.classList.add("on");
  positionContextMenu(menu, event);
});

document.getElementById("msg-copy").onclick = async () => {
  const message = ctxMessage;
  closeMessageCtx();
  if (!message) return;
  try {
    await navigator.clipboard.writeText(message.text);
  } catch (error) {
    feed.showStatus("Could not copy message: " + (error.message || String(error)));
  }
};

document.getElementById("msg-fork").onclick = () => {
  const message = ctxMessage;
  const forkTab = openForkTab();
  closeMessageCtx();
  if (!message) {
    forkTab?.close();
    return;
  }
  runConversationCommand({
    botId: message.botId,
    command: "fork",
    entryId: message.entryId,
    forkTab,
  }).catch((error) => {
    forkTab?.close();
    feed.showStatus(error.message || String(error));
  });
};
document.addEventListener("mousedown", (e) => {
  if (!e.target.closest("#ctx")) closeCtx();
  if (!e.target.closest("#msg-ctx")) closeMessageCtx();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    closeCtx();
    closeMessageCtx();
  }
});

document.getElementById("f-name").addEventListener("input", paintSheetAv);
document.getElementById("head").onclick = (e) => {
  if (e.target.closest("button, label")) return;
  const bot = bots.find((b) => b.id === current);
  if (bot) openSheet("edit", bot);
};
document.getElementById("bot-form").onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    name: document.getElementById("f-name").value.trim(),
    title: document.getElementById("f-title").value.trim(),
    description: document.getElementById("f-desc").value.trim(),
    avatar: avatarSpec(),
    model: document.getElementById("f-model").value || null,
    soul: document.getElementById("f-soul").value
  };
  if (!body.name) return;
  if (sheetMode === "new") {
    if (!body.soul.trim()) delete body.soul;
    const created = await api("/api/bots", { method: "POST", body: JSON.stringify(body) });
    closeSheet();
    await loadBots();
    if (created && created.id) select(created.id);
  } else {
    const id = sheetBotId || current;
    const { soul, ...profile } = body;
    await api("/api/bots/" + encodeURIComponent(id), { method: "PATCH", body: JSON.stringify(profile) });
    await api("/api/bots/" + encodeURIComponent(id) + "/soul", {
      method: "PUT", body: JSON.stringify({ text: soul })
    });
    closeSheet();
    await loadBots();
    paintHead(bots.find((b) => b.id === current));
  }
};

document.getElementById("new").onclick = createAgent;
document.getElementById("q").oninput = () => loadBots();
composer.addEventListener("reve:intent:new-bot", createAgent);

autocomplete.addEventListener("reve:intent:catalog", (event) => {
  const { requestId, trigger, botId } = event.detail;
  const signal = autocomplete.requestSignal(requestId);
  if (!signal) return;
  const path = trigger === "@" ? "/api/bots" : "/api/bots/" + encodeURIComponent(botId) + "/skills";
  api(path, { signal }, { requestId, topic: "catalog", owner: { botId } }).catch((error) => {
    if (error.name !== "AbortError") console.error(error);
  });
});

composer.addEventListener("reve:intent:attachment", (event) => {
  const { requestId, botId } = event.detail;
  const payload = composer.attachmentPayload(requestId);
  if (!payload) return;
  const { file, signal } = payload;
  const body = new FormData();
  body.append("file", file, file.name || "file");
  api(
    "/api/bots/" + encodeURIComponent(botId) + "/attachments",
    { method: "POST", headers: { "Authorization": "Bearer " + TOKEN }, body, signal },
    { requestId, topic: "attachment", owner: { botId } },
  ).catch((error) => {
    if (error.name !== "AbortError") console.error(error);
  });
});

feed.addEventListener("reve:intent:attachment-preview", (event) => {
  const target = event.target;
  const { requestId, botId, attachment } = event.detail;
  const previous = attachmentPreviewRequests.get(target);
  previous?.abort();
  const controller = new AbortController();
  attachmentPreviewRequests.set(target, controller);
  void (async () => {
    try {
      if (!attachment?.id || !attachment?.name) throw new Error("Attachment preview is unavailable");
      const path = "/api/bots/" + encodeURIComponent(botId) + "/attachments/" +
        encodeURIComponent(attachment.id) + "/" + encodeURIComponent(attachment.name);
      const response = await fetch(path, {
        headers: { "Authorization": "Bearer " + TOKEN },
        signal: controller.signal,
      });
      if (!response.ok) throw new Error(`Preview failed (${response.status})`);
      emitServerState({
        source: "http", requestId, topic: "attachment-preview", owner: { botId },
        path, method: "GET", ok: true, status: response.status,
        data: { text: await response.text() },
      });
    } catch (error) {
      if (error.name === "AbortError") return;
      emitServerState({
        source: "http", requestId, topic: "attachment-preview", owner: { botId },
        ok: false, status: error.status || 0, error: error.message || String(error),
      });
    } finally {
      if (attachmentPreviewRequests.get(target) === controller) {
        attachmentPreviewRequests.delete(target);
      }
    }
  })();
});

composer.addEventListener("reve:intent:send", (event) => {
  const { requestId, botId } = event.detail;
  const payload = composer.submissionPayload(requestId);
  if (!payload) return;
  const { text, rawText, attachmentCount } = payload;
  const command = parseConversationCommand(rawText, attachmentCount);
  const forkTab = command?.command === "fork" ? openForkTab() : null;
  void (async () => {
    try {
      if (command) {
        if (command.invalidArguments) {
          throw new Error(`/${command.command} does not accept arguments`);
        }
        const data = await runConversationCommand({
          botId,
          command: command.command,
          instructions: command.instructions,
          requestId,
          topic: "message-response",
          forkTab,
        });
        emitServerState({
          source: "projection", requestId, topic: "message-send",
          owner: { botId, conversationId: data.log_id }, ok: true, status: 200, data,
        });
        return;
      }
      let logId = logIds.get(botId) || bots.find((bot) => bot.id === botId)?.log_id;
      if (!logId) {
        await streamReady;
        logId = logIds.get(botId);
      }
      if (!logId) throw new Error("Connect to the bot log before sending");
      const ack = await api(
        "/api/bots/" + encodeURIComponent(botId) + "/messages",
        { method: "POST", body: JSON.stringify({ text, log_id: logId }) },
        { requestId, topic: "message-response", owner: { botId, conversationId: logId } },
      );
      const connectedLog = logIds.get(botId);
      if (ack.log_id !== logId || (connectedLog && ack.log_id !== connectedLog)) {
        throw new Error("Bot was replaced before the message settled");
      }
      if (!ack.record) throw new Error("Message was cancelled before placement");
      chatLog(botId).upsert(ack.record);
      if (current === botId) {
        feed.pinToBottom();
        queueLogRender(botId);
      }
      emitServerState({
        source: "projection", requestId, topic: "message-send", owner: { botId, conversationId: logId },
        ok: true, status: 200, data: ack,
      });
    } catch (error) {
      forkTab?.close();
      emitServerState({
        source: "projection", requestId, topic: "message-send",
        owner: { botId, conversationId: logIds.get(botId) || null },
        ok: false, status: error.status || 0, error: error.message || String(error),
      });
    }
  })();
});

feed.addEventListener("reve:intent:load-older", () => { void loadOlder(); });

function closeDrawers() {
  const nav = document.getElementById("nav-toggle");
  const meta = document.getElementById("meta-toggle");
  if (nav) nav.checked = false;
  if (meta) meta.checked = false;
}

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") closeDrawers();
});
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}
let installEvent = null;
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  installEvent = e;
  document.getElementById("install").classList.add("on");
});
document.getElementById("install").onclick = async () => {
  if (!installEvent) return;
  installEvent.prompt();
  await installEvent.userChoice.catch(() => {});
  installEvent = null;
  document.getElementById("install").classList.remove("on");
};
window.addEventListener("appinstalled", () => {
  document.getElementById("install").classList.remove("on");
});

let desktopUrl = null;
let desktopTaken = false;

async function loadDesktop() {
  const body = document.getElementById("desktop-body");
  const hint = document.getElementById("desktop-hint");
  try {
    const d = await api("/api/desktop");
    desktopUrl = d && d.novnc ? d.novnc : null;
    if (!desktopUrl) {
      body.innerHTML = '<div class="hint">House computer is the microVM.<br>/workspace is shared by every bot.</div>';
      if (hint) hint.textContent = "offline";
      return;
    }
    if (hint) hint.textContent = "click to take over";
    const frame = document.createElement("iframe");
    frame.id = "desktop-frame";
    frame.title = "microVM desktop";
    frame.allow = "clipboard-read; clipboard-write";
    frame.src = desktopUrl;
    body.innerHTML = "";
    body.appendChild(frame);
    if (desktopTaken) takeOverDesktop();
  } catch (e) {
    desktopUrl = null;
    body.innerHTML = '<div class="hint">Desktop unavailable.</div>';
    if (hint) hint.textContent = "offline";
  }
}

function desktopFrame() {
  return document.getElementById("desktop-frame");
}

function takeOverDesktop() {
  const frame = desktopFrame();
  const overlay = document.getElementById("desktop-takeover");
  const slot = document.getElementById("desktop-takeover-slot");
  const hint = document.getElementById("desktop-hint");
  if (!frame || !overlay || !slot || !desktopUrl) return;
  slot.appendChild(frame);
  overlay.classList.add("on");
  desktopTaken = true;
  if (hint) hint.textContent = "taken over";
}

function giveBackDesktop() {
  const frame = desktopFrame();
  const overlay = document.getElementById("desktop-takeover");
  const body = document.getElementById("desktop-body");
  const hint = document.getElementById("desktop-hint");
  if (overlay) overlay.classList.remove("on");
  if (frame && body) body.appendChild(frame);
  desktopTaken = false;
  if (hint) hint.textContent = "click to take over";
}

document.getElementById("desktop").addEventListener("click", takeOverDesktop);
document.getElementById("desktop-close").addEventListener("click", giveBackDesktop);
document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape" && desktopTaken && !document.getElementById("sheet").classList.contains("on")) {
    ev.preventDefault();
    giveBackDesktop();
  }
});

window.addEventListener("hashchange", () => {
  const id = routeBot();
  if (id && id !== current) select(id, true);
});
window.addEventListener("popstate", () => {
  const id = routeBot();
  if (id && id !== current) select(id, true);
});

document.querySelectorAll(".side-tab").forEach((t) => {
  t.onclick = () => showSide(t.dataset.pane);
});
document.getElementById("file-peek-close").onclick = () => {
  document.getElementById("file-peek").hidden = true;
};

document.getElementById("tree").addEventListener("keydown", treeKey);
document.getElementById("tree-refresh").onclick = () => {
  fsStat.clear();
  loadTree().catch(() => {});
};
document.getElementById("tree-collapse").onclick = collapseTree;

let hoverRaf = 0;
document.getElementById("log").addEventListener("mousemove", (e) => {
  if (hoverRaf) return;
  const x = e.clientX, y = e.clientY;
  hoverRaf = requestAnimationFrame(() => {
    hoverRaf = 0;
    inspectHover(x, y);
  });
});
document.getElementById("log").addEventListener("mouseleave", () => unwrapFileRef(liveRef));
document.getElementById("log").addEventListener("click", (e) => {
  const ref = e.target.closest && e.target.closest(".file-ref");
  if (!ref) return;
  e.preventDefault();
  revealFile(ref.dataset.path || "").catch(() => {});
});

ensureHouseEvents();
Promise.all([loadBots(), loadRoutines(), loadDesktop()]).catch((e) => {
  feed.showStatus(e.message);
});