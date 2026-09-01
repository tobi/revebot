import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

import * as ReveLog from "../public/js/lib/log.mjs";

const directory = path.dirname(fileURLToPath(import.meta.url));
const app = fs.readFileSync(path.join(directory, "../public/js/app.mjs"), "utf8");
const start = app.indexOf("// Browser adapter. Every transcript update");
const end = app.indexOf("\nfunction routeBot", start);
assert(start >= 0 && end > start, "conversation controller module block");

const sockets = [];
const requests = [];
const timers = [];
const serverStates = [];
const trace = [];

class Socket {
  constructor(url) {
    this.url = url;
    this.readyState = 1;
    sockets.push(this);
    trace.push("subscribe");
  }

  close() {
    this.readyState = 3;
    this.onclose?.();
  }

  emit(event) {
    this.onmessage?.({ data: JSON.stringify(event) });
  }
}

const composer = { botId: "", value: "", readOnly: false, setStatusline() {} };
const feed = {
  botId: "",
  loadingOlder: false,
  history: { hasMore: false, oldestSeq: null },
  setHistory(value) { this.history = value; },
  reset() { this.history = { hasMore: false, oldestSeq: null }; },
  beginRender() { return { height: 0, top: 0, pinned: true }; },
  finishRender() {},
  showStatus() {},
};
const context = vm.createContext({
  ReveLog,
  WebSocket: Socket,
  Promise,
  Map,
  Set,
  JSON,
  console,
  current: null,
  ws: null,
  frozenConversation: null,
  statuslines: {},
  bots: [
    { id: "a", name: "A", status: "ready" },
    { id: "b", name: "B", status: "ready" },
  ],
  location: { protocol: "http:", host: "fixture" },
  TOKEN: "fixture",
  PAGE: 80,
  composer,
  feed,
  api(url, options) {
    trace.push("snapshot");
    return new Promise((resolve, reject) => requests.push({ url, options, resolve, reject }));
  },
  emitServerState(detail) { serverStates.push(detail); },
  requestAnimationFrame() {},
  setTimeout(callback) { timers.push(callback); },
  setBusy() {},
  setRoute() {},
  resetTranscript() { feed.reset(); },
  paintHead() {},
  closeDrawers() {},
  loadBots: async () => {},
  loadRoutines: async () => {},
  el: (tag, className, text) => ({ tag, className, textContent: text || "" }),
  document: {},
});
vm.runInContext(app.slice(start, end), context);

const entry = (id, text, seq = 0) => ({
  id,
  seq,
  type: "message",
  display: { audience: "chat", run_id: "run" },
  message: { role: "assistant", content: [{ type: "text", text }], stopReason: "stop" },
});
const rows = (id) => vm.runInContext(`chatLog(${JSON.stringify(id)}).records()`, context);
const snapshot = (logId, records) => ({
  log_id: logId,
  records,
  has_more: false,
  oldest_seq: 1,
  operation_id: null,
});

const first = context.select("a");
assert.equal(requests.length, 0, "snapshot waits for subscribed log identity");
sockets[0].emit({ type: "hello", log_id: "session-a" });
assert.deepEqual(trace, ["subscribe", "snapshot"]);
sockets[0].emit({ type: "entry_draft", entry: entry("reply", "old"), order: 2, version: 1 });
requests.shift().resolve(snapshot("session-a", [{
  entry: entry("reply", "latest"),
  status: "streaming",
  order: 2,
  revision: 10,
}]));
await first;
assert.equal(ReveLog.text(rows("a")[0].entry.message.content), "latest", "buffered old draft cannot shrink snapshot");
sockets[0].emit({ type: "entry_added", entry: entry("reply", "latest complete", 3) });
assert.equal(rows("a")[0].status, "committed");

const second = context.select("b");
sockets[1].emit({ type: "hello", log_id: "session-b" });
const bRequest = requests.shift();
sockets[0].emit({ type: "entry_added", entry: entry("poison", "wrong bot", 99) });
assert.equal(rows("b").length, 0);
assert.equal(serverStates.at(-1).stale, true, "abandoned socket frames are observable but marked stale");

const third = context.select("a");
sockets[2].emit({ type: "hello", log_id: "session-a" });
bRequest.resolve(snapshot("session-b", [{ entry: entry("late-b", "late", 1), status: "committed", order: 1, revision: 0 }]));
requests.shift().resolve(snapshot("session-a", [{ entry: entry("reply", "latest complete", 3), status: "committed", order: 3, revision: 0 }]));
await Promise.all([second, third]);
assert.equal(context.current, "a");
assert.equal(rows("a").length, 1);
assert.equal(rows("b").length, 0);

composer.value = "must not reach replacement";
sockets[2].close();
assert.equal(serverStates.at(-1).topic, "bot-connection");
assert.equal(serverStates.at(-1).ok, false);
timers.shift()();
sockets[3].emit({ type: "hello", log_id: "replacement-a" });
requests.shift().resolve(snapshot("replacement-a", [{ entry: entry("new", "new bot", 1), status: "committed", order: 1, revision: 0 }]));
await new Promise((resolve) => setImmediate(resolve));
assert.equal(rows("a").length, 1);
assert.equal(rows("a")[0].entry.id, "new");
assert.equal(composer.value, "");
sockets[2].emit({ type: "entry_added", entry: entry("old", "old incarnation", 100) });
assert.equal(rows("a").length, 1);
