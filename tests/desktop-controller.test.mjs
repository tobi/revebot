import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const directory = path.dirname(fileURLToPath(import.meta.url));
const appSource = fs.readFileSync(path.join(directory, "../public/js/app.mjs"), "utf8");
const desktopStart = appSource.indexOf("let desktopUrl = null;");
const desktopEnd = appSource.indexOf('\nwindow.addEventListener("hashchange"', desktopStart);
const previewStart = appSource.indexOf("async function previewFile(path)");
const previewEnd = appSource.indexOf("\nfunction showSide", previewStart);
assert(desktopStart >= 0 && desktopEnd > desktopStart, "desktop controller block");
assert(previewStart >= 0 && previewEnd > previewStart, "file preview controller block");

class TestElement extends EventTarget {
  constructor(document, tag = "div", id = "") {
    super();
    this.document = document;
    this.tag = tag;
    this.children = [];
    this.parentNode = null;
    this.attributes = new Map();
    this.className = "";
    this.disabled = false;
    this.hidden = false;
    this.inert = false;
    this.open = false;
    this.classList = {
      add: (...names) => {
        const values = new Set(this.className.split(" ").filter(Boolean));
        for (const name of names) values.add(name);
        this.className = [...values].join(" ");
      },
      remove: (...names) => {
        const removed = new Set(names);
        this.className = this.className.split(" ").filter((name) => name && !removed.has(name)).join(" ");
      },
      contains: (name) => this.className.split(" ").includes(name),
    };
    if (id) this.id = id;
  }

  set id(value) {
    if (this._id) this.document.elements.delete(this._id);
    this._id = value;
    if (value) this.document.elements.set(value, this);
  }

  get id() { return this._id || ""; }

  appendChild(node) {
    if (node.parentNode) node.parentNode.children = node.parentNode.children.filter((child) => child !== node);
    node.parentNode = this;
    this.children.push(node);
    return node;
  }

  replaceChildren(...nodes) {
    for (const child of this.children) child.parentNode = null;
    this.children = [];
    for (const node of nodes) this.appendChild(node);
  }

  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.get(name) ?? null; }
  focus() { this.document.focused = this; }
  showModal() { this.open = true; }
  close() { this.open = false; }
}


class TestDocument extends EventTarget {
  constructor() {
    super();
    this.elements = new Map();
    this.focused = null;
  }

  createElement(tag) { return new TestElement(this, tag); }
  getElementById(id) { return this.elements.get(id) || null; }
  add(id, tag = "div") { return new TestElement(this, tag, id); }
}

const document = new TestDocument();
const body = document.add("desktop-body");
const hint = document.add("desktop-hint");
const trigger = document.add("desktop-open", "button");
const close = document.add("desktop-close", "button");
const screen = document.add("desktop", "dialog");
document.add("app");
const peek = document.add("file-peek");
const peekBody = document.add("file-peek-body", "pre");
const peekName = document.add("file-peek-name", "span");
const calls = [];

const context = vm.createContext({
  document,
  window: new EventTarget(),
  Event,
  console,
  api: async (url) => {
    calls.push(url);
    if (url === "/api/desktop") return { novnc: "http://127.0.0.1:7608/vnc.html" };
    if (url.startsWith("/api/fs/file")) return { text: "file body", binary: false, truncated: false };
    throw new Error(`unexpected API call: ${url}`);
  },
  el: (tag, className, text) => {
    const node = document.createElement(tag);
    node.className = className || "";
    node.textContent = text || "";
    return node;
  },
});
vm.runInContext(appSource.slice(desktopStart, desktopEnd), context);
vm.runInContext(appSource.slice(previewStart, previewEnd), context);

await context.loadDesktop();
assert.deepEqual(calls, ["/api/desktop"]);
assert.equal(body.children.length, 1);
const frame = body.children[0];
assert.equal(frame.id, "desktop-frame");
assert.equal(frame.src, "http://127.0.0.1:7608/vnc.html");
assert.equal(trigger.disabled, false);
frame.dispatchEvent(new Event("load"));
assert.equal(hint.textContent, "live preview");

trigger.dispatchEvent(new Event("click"));
assert.equal(body.children[0], frame, "takeover preserves the iframe parent and live connection");
assert.equal(screen.open, true);
assert.equal(trigger.getAttribute("aria-expanded"), "true");
assert.equal(document.focused, close);

close.dispatchEvent(new Event("click"));
assert.equal(body.children[0], frame);
assert.equal(screen.open, false);
assert.equal(trigger.getAttribute("aria-expanded"), "false");
assert.equal(document.focused, trigger);

await context.previewFile("notes/report.txt");
assert.equal(body.children[0], frame, "file preview cannot replace the live desktop iframe");
assert.equal(peek.hidden, false);
assert.equal(peekName.textContent, "/workspace/notes/report.txt");
assert.equal(peekBody.textContent, "file body");
