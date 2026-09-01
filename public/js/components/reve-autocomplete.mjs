import { emitIntent, nextRequestId, SERVER_STATE_EVENT } from "../events.mjs";
import { BLOUB } from "../lib/bloub.mjs";
import { CONVERSATION_COMMANDS } from "../conversation-commands.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function tokenAt(value, caret) {
  const before = value.slice(0, caret);
  const match = before.match(/(^|[\s])([/@])([^\n]*)$/);
  if (!match) return null;
  const trigger = match[2];
  const query = match[3];
  if (trigger === "/" && /[^a-zA-Z0-9_-]/.test(query)) return null;
  return { trigger, query, start: before.length - query.length - 1 };
}

function score(query, fields) {
  if (!query) return 1;
  let best = 0;
  for (const field of fields) {
    const value = String(field || "").toLowerCase();
    if (!value) continue;
    if (value === query) best = Math.max(best, 100);
    else if (value.startsWith(query)) best = Math.max(best, 80);
    else if (value.includes(query)) best = Math.max(best, 40);
  }
  return best;
}

const CONVERSATION_COMMAND_NAMES = new Set(CONVERSATION_COMMANDS.map((command) => command.name));

function slashCatalog(skills) {
  return [
    ...CONVERSATION_COMMANDS,
    ...(skills || []).filter((skill) => !CONVERSATION_COMMAND_NAMES.has(skill.name)),
  ];
}

function matchesResponse(detail, requestId, sourceKey, currentKey) {
  return Boolean(
    detail
    && detail.topic === "catalog"
    && detail.requestId === requestId
    && sourceKey
    && sourceKey === currentKey,
  );
}

function marked(text, query) {
  const span = document.createElement("span");
  if (!query) {
    span.textContent = text;
    return span;
  }
  const position = String(text).toLowerCase().indexOf(query.toLowerCase());
  if (position < 0) {
    span.textContent = text;
    return span;
  }
  span.append(document.createTextNode(text.slice(0, position)));
  const bold = document.createElement("b");
  bold.textContent = text.slice(position, position + query.length);
  span.append(bold, document.createTextNode(text.slice(position + query.length)));
  return span;
}

function avatar(bot) {
  const node = element("div", "av sm");
  node.dataset.bot = bot.id || "";
  node.innerHTML = BLOUB.svg(bot.avatar, bot.id || bot.name, 28);
  return node;
}

export class ReveAutocomplete extends HTMLElement {
  #input = null;
  #botId = "";
  #sourceKey = "";
  #requestId = "";
  #abort = null;
  #catalog = [];
  #items = [];
  #selected = 0;
  #token = null;
  #loading = false;
  #error = "";
  #blurTimer = 0;

  connectedCallback() {
    this.hidden = true;
    this.classList.add("ac");
    this.setAttribute("role", "listbox");
    document.addEventListener(SERVER_STATE_EVENT, this.#onServerState);
    this.addEventListener("mousedown", this.#onPointerDown);
    this.addEventListener("mousemove", this.#onPointerMove, { passive: true });
    document.addEventListener("mousedown", this.#onOutsidePointerDown);
    window.addEventListener("resize", this.#position, { passive: true });
  }

  disconnectedCallback() {
    this.detach();
    document.removeEventListener(SERVER_STATE_EVENT, this.#onServerState);
    this.removeEventListener("mousedown", this.#onPointerDown);
    this.removeEventListener("mousemove", this.#onPointerMove);
    document.removeEventListener("mousedown", this.#onOutsidePointerDown);
    window.removeEventListener("resize", this.#position);
  }

  attach(input) {
    if (this.#input === input) return;
    this.detach();
    this.#input = input;
    input.addEventListener("input", this.#update);
    input.addEventListener("click", this.#update);
    input.addEventListener("keyup", this.#onKeyUp);
    input.addEventListener("keydown", this.#onKeyDown);
    input.addEventListener("blur", this.#onBlur);
  }

  detach() {
    if (!this.#input) return;
    this.#input.removeEventListener("input", this.#update);
    this.#input.removeEventListener("click", this.#update);
    this.#input.removeEventListener("keyup", this.#onKeyUp);
    this.#input.removeEventListener("keydown", this.#onKeyDown);
    this.#input.removeEventListener("blur", this.#onBlur);
    this.#input = null;
    this.close();
  }

  set botId(value) {
    const next = value || "";
    if (next === this.#botId) return;
    this.#botId = next;
    this.close();
  }

  get botId() {
    return this.#botId;
  }

  get active() {
    return !this.hidden;
  }

  acceptIfAvailable() {
    if (!this.active || !this.#items.length) return false;
    this.#accept();
    return true;
  }


  requestSignal(requestId) {
    return requestId === this.#requestId ? this.#abort?.signal || null : null;
  }
  close() {
    this.#abort?.abort();
    this.#abort = null;
    this.#sourceKey = "";
    this.#requestId = "";
    this.#catalog = [];
    this.#items = [];
    this.#token = null;
    this.#loading = false;
    this.#error = "";
    this.#selected = 0;
    this.hidden = true;
    this.classList.remove("on");
    this.replaceChildren();
  }

  #currentToken() {
    if (!this.#input) return null;
    return tokenAt(this.#input.value, this.#input.selectionStart);
  }

  #key(token) {
    return `${this.#botId}\n${token.trigger}\n${token.start}`;
  }

  #update = () => {
    const token = this.#currentToken();
    if (!token) {
      this.close();
      return;
    }
    const key = this.#key(token);
    this.#token = token;
    if (key !== this.#sourceKey) {
      this.#beginRequest(token, key);
      return;
    }
    this.#filterAndRender();
  };

  #beginRequest(token, key) {
    this.#abort?.abort();
    this.#abort = new AbortController();
    this.#sourceKey = key;
    this.#requestId = nextRequestId("autocomplete");
    this.#catalog = [];
    this.#items = [];
    this.#loading = true;
    this.#error = "";
    this.#render();
    emitIntent(this, "catalog", {
      requestId: this.#requestId,
      trigger: token.trigger,
      botId: this.#botId,
    });
  }

  #onServerState = (event) => {
    const detail = event.detail;
    const token = this.#currentToken();
    const currentKey = token ? this.#key(token) : "";
    if (!matchesResponse(detail, this.#requestId, this.#sourceKey, currentKey)) return;
    if (!detail.ok) {
      this.#loading = false;
      this.#error = detail.error || "Could not refresh autocomplete";
      this.#items = [];
      this.#render();
      return;
    }
    this.#catalog = token.trigger === "@" ? detail.data.bots || [] : slashCatalog(detail.data.skills);
    this.#loading = false;
    this.#error = "";
    this.#token = token;
    this.#filterAndRender();
  };

  #filterAndRender() {
    if (!this.#token || this.#loading || this.#error) {
      this.#render();
      return;
    }
    const query = this.#token.query.toLowerCase();
    const items = [];
    if (this.#token.trigger === "@") {
      for (const bot of this.#catalog) {
        if (bot.id === this.#botId) continue;
        const rank = score(query, [bot.name, bot.id, bot.title, bot.description]);
        if (rank) items.push({ kind: "bot", rank, name: bot.name, insert: `@${bot.name} `, bot });
      }
    } else {
      for (const entry of this.#catalog) {
        const rank = score(query, [entry.name, entry.description]);
        if (!rank) continue;
        const command = entry.source === "command";
        const suffix = command && !entry.acceptsArguments ? "" : " ";
        items.push({
          kind: command ? "command" : "skill",
          rank,
          name: entry.name,
          insert: `/${entry.name}${suffix}`,
          description: entry.description,
        });
      }
    }
    items.sort((left, right) => right.rank - left.rank || left.name.localeCompare(right.name));
    this.#items = items.slice(0, 40);
    this.#selected = Math.min(this.#selected, Math.max(0, this.#items.length - 1));
    this.#render();
  }

  #render() {
    const token = this.#token;
    if (!token) return;
    const fragment = document.createDocumentFragment();
    fragment.append(element("div", "ac-cap", token.trigger === "@" ? "Bots" : "Commands"));
    if (this.#loading) {
      fragment.append(element("div", "ac-empty", "Refreshing…"));
    } else if (this.#error) {
      fragment.append(element("div", "ac-empty", this.#error));
    } else if (!this.#items.length) {
      fragment.append(element("div", "ac-empty", token.trigger === "@" ? "No matching bots" : "No matching commands"));
    } else {
      for (const [index, item] of this.#items.entries()) {
        const row = element("div", `ac-item${index === this.#selected ? " on" : ""}`);
        row.dataset.index = String(index);
        row.setAttribute("role", "option");
        row.setAttribute("aria-selected", index === this.#selected ? "true" : "false");
        if (item.kind === "bot") row.append(avatar(item.bot));
        const meta = element("div", "meta");
        const name = element("div", "k");
        name.append(element("span", "slash", token.trigger), marked(item.name, token.query));
        meta.append(name);
        const description = item.kind === "bot"
          ? item.bot.title || item.bot.description || item.bot.id
          : item.description;
        if (description) meta.append(element("div", "d", description));
        row.append(meta);
        fragment.append(row);
      }
    }
    const hint = element("div", "ac-hint");
    hint.append(
      element("span", "", token.trigger === "@" ? "@ mention a teammate" : "/ command or skill"),
      element("span", "", "↑↓  Tab"),
    );
    fragment.append(hint);
    this.replaceChildren(fragment);
    this.hidden = false;
    this.classList.add("on");
    this.#position();
  }

  #highlight() {
    const rows = this.querySelectorAll(".ac-item");
    for (const [index, row] of rows.entries()) {
      const selected = index === this.#selected;
      row.classList.toggle("on", selected);
      row.setAttribute("aria-selected", selected ? "true" : "false");
      if (selected) row.scrollIntoView({ block: "nearest" });
    }
  }

  #move(delta) {
    if (!this.#items.length) return;
    this.#selected = (this.#selected + delta + this.#items.length) % this.#items.length;
    this.#highlight();
  }

  #accept() {
    if (!this.#input || !this.#token || !this.#items.length) return;
    const item = this.#items[this.#selected];
    if (!item) return;
    const end = this.#input.selectionStart;
    const caret = this.#token.start + item.insert.length;
    this.#input.value = this.#input.value.slice(0, this.#token.start) + item.insert + this.#input.value.slice(end);
    this.#input.setSelectionRange(caret, caret);
    this.#input.focus();
    this.close();
  }

  #onKeyDown = (event) => {
    if (!this.active || event.isComposing) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      this.#move(event.key === "ArrowDown" ? 1 : -1);
    } else if ((event.key === "Enter" || event.key === "Tab") && this.#items.length) {
      event.preventDefault();
      this.#accept();
    } else if (event.key === "Tab") {
      event.preventDefault();
      this.close();
    } else if (event.key === "Escape") {
      event.preventDefault();
      this.close();
    }
  };

  #onKeyUp = (event) => {
    if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) this.#update();
  };

  #onBlur = () => {
    clearTimeout(this.#blurTimer);
    this.#blurTimer = window.setTimeout(() => {
      if (!this.matches(":hover")) this.close();
    }, 120);
  };

  #onPointerDown = (event) => {
    const row = event.target.closest(".ac-item");
    if (!row) return;
    event.preventDefault();
    const index = Number(row.dataset.index);
    if (!Number.isInteger(index) || !this.#items[index]) return;
    this.#selected = index;
    this.#accept();
  };

  #onPointerMove = (event) => {
    const row = event.target.closest(".ac-item");
    if (!row) return;
    const index = Number(row.dataset.index);
    if (!Number.isInteger(index) || index === this.#selected || !this.#items[index]) return;
    this.#selected = index;
    this.#highlight();
  };

  #onOutsidePointerDown = (event) => {
    if (!this.active || event.target === this.#input || this.contains(event.target)) return;
    this.close();
  };

  #position = () => {
    if (this.hidden || !this.#input) return;
    if (globalThis.CSS?.supports?.("position-anchor: --compose-box")) {
      this.style.left = "";
      this.style.top = "";
      this.style.width = "";
      return;
    }
    const box = this.#input.closest(".box")?.getBoundingClientRect();
    if (!box) return;
    const width = Math.min(440, Math.max(280, box.width));
    const height = this.offsetHeight || 180;
    const left = Math.max(12, Math.min(box.left, window.innerWidth - width - 12));
    this.style.width = `${width}px`;
    this.style.left = `${left}px`;
    this.style.top = `${Math.max(8, box.top - height - 8)}px`;
  };
}

customElements.define("reve-autocomplete", ReveAutocomplete);

export { matchesResponse, score, slashCatalog, tokenAt };
