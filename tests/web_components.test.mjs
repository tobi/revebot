import assert from "node:assert/strict";
import test from "node:test";

class TestElement extends EventTarget {
  constructor(tag = "custom-element", text = "") {
    super();
    this.tag = tag;
    this.children = [];
    this.className = "";
    this.dataset = {};
    this.attributes = new Map();
    this._text = text;
    this.isConnected = false;
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
  }

  appendChild(node) {
    node.parentElement = this;
    this.children.push(node);
    return node;
  }

  append(...nodes) {
    for (const node of nodes) this.appendChild(node);
  }

  replaceChildren(...nodes) {
    this.children = [];
    this.append(...nodes);
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
    if (name === "open") this.open = true;
  }

  querySelector(selector) {
    if (!selector.startsWith(".")) return null;
    const className = selector.slice(1);
    return this.descendants().find((node) => node.className.split(" ").includes(className)) || null;
  }

  descendants() {
    return this.children.flatMap((child) => [child, ...child.descendants()]);
  }

  showModal() { this.open = true; }
  close() { this.open = false; }

  get textContent() {
    return this._text + this.children.map((child) => child.textContent).join("");
  }

  set textContent(value) {
    this._text = String(value);
    this.children = [];
  }
}
class TestCustomEvent extends Event {
  constructor(type, options = {}) {
    super(type, options);
    this.detail = options.detail;
  }
}

const definitions = new Map();
globalThis.HTMLElement = TestElement;
globalThis.CustomEvent ??= TestCustomEvent;
globalThis.customElements = {
  define(name, constructor) {
    assert.equal(definitions.has(name), false, `${name} registered twice`);
    definitions.set(name, constructor);
  },
};
globalThis.document = Object.assign(new EventTarget(), {
  createElement: (tag) => new TestElement(tag),
  createTextNode: (text) => new TestElement("#text", text),
  createDocumentFragment: () => new TestElement("#fragment"),
});
globalThis.window = new EventTarget();

const events = await import("../public/js/events.mjs");
const autocomplete = await import("../public/js/components/reve-autocomplete.mjs");
const conversationCommands = await import("../public/js/conversation-commands.mjs");
const attachment = await import("../public/js/components/reve-attachment.mjs");
await import("../public/js/components/reve-feed.mjs");
await import("../public/js/components/reve-composer.mjs");

test("public components register once under stable names", () => {
  assert.deepEqual([...definitions.keys()].sort(), [
    "reve-attachment",
    "reve-autocomplete",
    "reve-composer",
    "reve-feed",
  ]);
});

test("server state dispatch preserves response identity", () => {
  const data = { bots: [{ id: "helper" }] };
  const detail = {
    source: "http",
    requestId: "catalog:8",
    topic: "catalog",
    owner: { botId: "main" },
    ok: true,
    status: 200,
    data,
  };
  let observed;
  const stop = events.onServerState((event) => { observed = event.detail; });
  assert.equal(events.emitServerState(detail), data);
  stop();
  assert.equal(observed, detail);
});

test("intent events carry correlation without transport globals", () => {
  const target = new EventTarget();
  let observed;
  target.addEventListener("reve:intent:catalog", (event) => { observed = event.detail; });
  const detail = { requestId: events.nextRequestId("catalog"), trigger: "/", botId: "main" };
  events.emitIntent(target, "catalog", detail);
  assert.equal(observed, detail);
  assert.notEqual(events.nextRequestId("catalog"), detail.requestId);
});

test("autocomplete tokenization and ranking are deterministic", () => {
  assert.deepEqual(autocomplete.tokenAt("hello @mi", 9), { trigger: "@", query: "mi", start: 6 });
  assert.deepEqual(autocomplete.tokenAt("/build", 6), { trigger: "/", query: "build", start: 0 });
  assert.equal(autocomplete.tokenAt("/not valid", 10), null);
  assert.equal(autocomplete.score("mi", ["Miku", "helper"]), 80);
  assert.equal(autocomplete.score("missing", ["Miku", "helper"]), 0);
});

test("slash autocomplete includes every executable conversation command", () => {
  assert.deepEqual(
    conversationCommands.CONVERSATION_COMMANDS.map((command) => command.name),
    ["compact", "new", "fork"],
  );
  assert.deepEqual(
    autocomplete.slashCatalog([
      { name: "build", description: "Build the project", source: "workspace" },
      { name: "compact", description: "Shadowed skill", source: "workspace" },
    ]).map((entry) => entry.name),
    ["compact", "new", "fork", "build"],
  );
  assert.deepEqual(conversationCommands.parseConversationCommand("/compact keep paths", 0), {
    command: "compact",
    instructions: "keep paths",
    invalidArguments: false,
  });
  assert.equal(conversationCommands.parseConversationCommand("/build", 0), null);
  assert.equal(conversationCommands.parseConversationCommand("/new unexpected", 0).invalidArguments, true);
});

test("autocomplete rejects stale and cross-request responses", () => {
  const response = { topic: "catalog", requestId: "autocomplete:2", ok: true };
  assert.equal(autocomplete.matchesResponse(response, "autocomplete:2", "bot-a\n@\n0", "bot-a\n@\n0"), true);
  assert.equal(autocomplete.matchesResponse(response, "autocomplete:1", "bot-a\n@\n0", "bot-a\n@\n0"), false);
  assert.equal(autocomplete.matchesResponse(response, "autocomplete:2", "bot-a\n@\n0", "bot-b\n@\n0"), false);
  assert.equal(autocomplete.matchesResponse({ ...response, topic: "bots" }, "autocomplete:2", "bot-a\n@\n0", "bot-a\n@\n0"), false);
});

test("attachment cards classify previews and build authenticated downloads", () => {
  assert.equal(attachment.kindOf({ name: "photo.bin", mime: "image/png" }), "image");
  assert.equal(attachment.kindOf({ name: "report.md", mime: "text/plain" }), "markdown");
  assert.equal(attachment.kindOf({ name: "page.html", mime: "text/html" }), "html");
  assert.equal(attachment.kindOf({ name: "archive.zip", mime: "application/zip" }), "file");
  assert.equal(attachment.formatBytes(1536), "1.5 KB");
  assert.equal(
    attachment.attachmentUrl({ id: "asset", name: "report.md" }, "bot one", "secret/token"),
    "/api/bots/bot%20one/attachments/asset/report.md?token=secret%2Ftoken",
  );
});

test("attachment preview intent and response stay correlated", () => {
  const Attachment = definitions.get("reve-attachment");
  const card = new Attachment();
  card.isConnected = true;
  card.data = {
    attachment: { id: "asset", name: "report.md", mime: "text/markdown", bytes: 42 },
    botId: "bot",
    token: "secret",
  };
  let intent;
  card.addEventListener("reve:intent:attachment-preview", (event) => { intent = event.detail; });
  card.connectedCallback();
  card.querySelector(".attachment-action").dispatchEvent(new Event("click"));
  assert.equal(intent.botId, "bot");
  assert.deepEqual(intent.attachment, { id: "asset", name: "report.md", url: "" });
  assert.equal(JSON.stringify(intent).includes("secret"), false);
  document.dispatchEvent(new CustomEvent(events.SERVER_STATE_EVENT, { detail: {
    source: "http",
    topic: "attachment-preview",
    requestId: intent.requestId,
    owner: { botId: "bot" },
    ok: true,
    status: 200,
    data: { text: "# Report\n\n**Ready**" },
  } }));
  assert.equal(card.querySelector(".attachment-lightbox").open, true);
  assert(card.querySelector(".attachment-markdown"));
  assert.equal(card.querySelector(".download").href.includes("token=secret"), true);
  card.disconnectedCallback();
});

test("composer exposes raw slash commands and can freeze a fork source tab", () => {
  const Composer = definitions.get("reve-composer");
  const composer = new Composer();
  composer.connectedCallback();
  composer.botId = "bot";
  assert.equal(composer.input.placeholder.includes("/ commands"), true);
  composer.value = "  /compact keep exact paths  ";
  let intent;
  composer.addEventListener("reve:intent:send", (event) => { intent = event.detail; });
  const form = composer.descendants().find((node) => node.tag === "form");
  form.dispatchEvent(new Event("submit", { cancelable: true }));

  assert.equal(intent.botId, "bot");
  assert.deepEqual(composer.submissionPayload(intent.requestId), {
    text: "/compact keep exact paths",
    rawText: "/compact keep exact paths",
    attachmentCount: 0,
  });
  composer.readOnly = true;
  assert.equal(composer.readOnly, true);
  assert.equal(composer.input.placeholder.includes("read-only"), true);
  document.dispatchEvent(new CustomEvent(events.SERVER_STATE_EVENT, { detail: {
    topic: "message-send",
    requestId: intent.requestId,
    ok: true,
  } }));
  assert.equal(composer.submissionPayload(intent.requestId), null);
  composer.disconnectedCallback();
});
