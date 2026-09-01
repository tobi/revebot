import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

import * as ReveLog from "../public/js/lib/log.mjs";

const directory = path.dirname(fileURLToPath(import.meta.url));
const app = fs.readFileSync(path.join(directory, "../public/js/app.mjs"), "utf8");
const entry = (id, text, audience = "chat", seq = 0) => ({
  id,
  seq,
  type: "message",
  display: { run_id: "run", audience },
  message: { role: "assistant", content: [{ type: "text", text }], stopReason: "stop" },
});
const committed = (value) => ({ entry: value, status: "committed", order: value.seq, revision: 0 });
const draft = (value, version) => ({ type: "entry_draft", entry: value, order: 2, version });

const log = new ReveLog.Log();
log.event(draft(entry("reply", "**Hello**"), 2));
assert.equal(ReveLog.project(log.records())[0].text, "**Hello**");
log.event({ type: "entry_added", entry: entry("reply", "**Hello**", "chat", 10) });
log.merge([committed(entry("reply", "**Hello**", "chat", 10))]);
log.event(draft(entry("reply", "stale"), 1));
assert.equal(log.records().length, 1);
assert.equal(ReveLog.project(log.records())[0].text, "**Hello**");
const fresh = new ReveLog.Log();
fresh.merge(log.records());
assert.deepEqual(ReveLog.project(fresh.records()), ReveLog.project(log.records()));

log.merge([
  committed({ id: "steer", seq: 11, type: "message", message: { role: "user", content: "another question" } }),
  committed(entry("internal", "private prose", "internal", 12)),
]);
assert(ReveLog.project(log.records()).some((row) => row.kind === "assistant" && row.text === "private prose"));
log.merge([committed({
  id: "intent",
  seq: 13,
  type: "message",
  message: { role: "assistant", content: [{ type: "toolCall", name: "SendUserMessage", arguments: { text: "NOT SENT" } }] },
})]);
assert(!ReveLog.project(log.records()).some((row) => row.text === "NOT SENT"));
const notice = { id: "notice", seq: 20, type: "custom", customType: "user_notice", data: { text: "Delivered" } };
log.event({ type: "entry_accepted", entry: notice, order: 14 });
log.event({ type: "entry_added", entry: notice });
log.merge([committed(notice)]);
assert.equal(ReveLog.project(log.records()).filter((row) => row.text === "Delivered").length, 1);
const fileNotice = {
  id: "file-notice", seq: 21, type: "custom", customType: "user_notice",
  data: { text: "", attachments: [{ id: "asset", name: "report.md", mime: "text/markdown", bytes: 42 }] },
};
log.merge([committed(fileNotice)]);
const fileRow = ReveLog.project(log.records()).find((row) => row.id === "file-notice");
assert.equal(fileRow.text, "");
assert.deepEqual(fileRow.attachments, fileNotice.data.attachments);

const resumed = new ReveLog.Log();
resumed.event(draft(entry("draft", "current partial"), 10));
resumed.event(draft(entry("draft", "old"), 3));
assert.equal(ReveLog.project(resumed.records())[0].text, "current partial");
resumed.interrupt();
assert.equal(ReveLog.project(resumed.records())[0].label, "Interrupted draft · not persisted");
resumed.event({
  type: "entry_added",
  entry: { ...entry("draft", "", "chat", 30), message: { role: "assistant", content: [], stopReason: "aborted", errorMessage: "Stopped" } },
});
assert.equal(ReveLog.project(resumed.records())[0].text, "current partial");
assert(ReveLog.project(resumed.records())[0].label.includes("not persisted"));

const tools = Array.from({ length: 12 }, (_, index) => ({
  id: `t${index}`,
  kind: "tool",
  run: "run",
  name: index < 7 ? "read" : "ls",
  args: { path: "/workspace/file" },
  text: "large output",
  running: false,
  failed: false,
}));
const groups = ReveLog.group([...tools.slice(0, 5), { id: "n", kind: "assistant", text: "Update" }, ...tools.slice(5)]);
assert.equal(groups.filter((row) => row.kind === "activity").length, 1);
assert.equal(ReveLog.summary(groups[0].tools), "Read 7 files · Listed 5 directories");
assert.equal(ReveLog.action({ name: "bash", args: { description: "Running tests" } }), "Running tests");
assert.equal(ReveLog.group([{ kind: "secret", id: "ask", run: "run" }, ...tools])[0].kind, "secret");

function extract(source, name) {
  let start = source.indexOf(`async function ${name}(`);
  if (start < 0) start = source.indexOf(`function ${name}(`);
  assert(start >= 0, name);
  return source.slice(start, source.indexOf("\n}", start) + 2);
}

class Element {
  constructor(tag, text = "") {
    this.tag = tag;
    this.children = [];
    this._text = text;
    this.className = "";
    this.dataset = {};
    this.style = {};
  }

  appendChild(node) {
    node.parentElement = this;
    this.children.push(node);
    return node;
  }

  get textContent() {
    return this._text + this.children.map((child) => child.textContent).join("");
  }

  set textContent(text) {
    this._text = String(text);
    this.children = [];
  }

  querySelector(selector) {
    return this.descendants().find((node) => node.className.split(" ").includes(selector.slice(1))) || null;
  }

  descendants() {
    return this.children.flatMap((child) => [child, ...child.descendants()]);
  }

  get classList() {
    return {
      add: (name) => { this.className += ` ${name}`; },
      contains: (name) => this.className.split(" ").includes(name),
    };
  }
}

const document = {
  createElement: (tag) => new Element(tag),
  createTextNode: (text) => new Element("#text", text),
};
globalThis.document = document;
const { formatText: sharedMarkdown } = await import("../public/js/lib/markdown.mjs");
const context = vm.createContext({
  document,
  URL,
  current: "bot",
  TOKEN: "test",
  items: [],
  rowCache: new Map(),
  avatar: () => new Element("avatar"),
  ReveLog,
  renderMarkdown: sharedMarkdown,
});
context.track = (node) => {
  context.items.push({ node });
  return node;
};
context.feed = {
  get itemCount() { return context.items.length; },
  itemsFrom(index) { return context.items.slice(index); },
  restore(items) { context.items.push(...items); },
};
for (const name of ["el", "fileAttrs", "filePill", "formatText", "addMessage", "unwrapUser"]) {
  vm.runInContext(extract(app, name), context);
}
vm.runInContext(extract(app, "paintLog"), context);

const markdown = "# Heading\n\n- first\n- second\n\n**bold** and `code`\n\n```rust\nfn main() {\n  println!(\"hello\");\n";
for (let length = 1; length <= markdown.length; length += 1) context.formatText(markdown.slice(0, length));
const dom = context.formatText(markdown);
const tags = [dom, ...dom.descendants()].map((node) => node.tag);
for (const tag of ["h1", "ul", "li", "strong", "code", "pre"]) assert(tags.includes(tag), tag);
assert(dom.textContent.includes('println!("hello");'));
const unsafeDom = context.formatText("<script>alert(1)</script>");
assert(![unsafeDom, ...unsafeDom.descendants()].some((node) => node.tag === "script"));

const bot = { id: "bot", name: "Bot" };
context.paintLog(bot, [{ id: "m", kind: "assistant", status: "streaming", text: markdown }]);
const partialBody = context.items[0].node.textContent;
context.items = [];
context.rowCache.clear();
context.paintLog(bot, [{ id: "m", kind: "assistant", status: "committed", text: markdown }]);
assert.equal(context.items[0].node.textContent, partialBody);

context.items = [];
context.rowCache.clear();
const attachment = { id: "asset", name: "report.md", mime: "text/markdown", bytes: 42 };
context.paintLog(bot, [{ id: "attachment", kind: "assistant", status: "committed", text: "", attachments: [attachment] }]);
const attachmentNode = context.items[0].node.descendants().find((node) => node.tag === "reve-attachment");
assert(attachmentNode);
assert.equal(attachmentNode.data.attachment, attachment);
assert.equal(attachmentNode.data.botId, "bot");
