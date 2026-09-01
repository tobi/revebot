import { emitIntent, SERVER_STATE_EVENT } from "../events.mjs";

const TAIL_ITEMS = 40;
const PIN_WINDOW = 80;
const ESTIMATED_ITEM_HEIGHT = 56;

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

export class ReveFeed extends HTMLElement {
  #items = [];
  #pinned = true;
  #unread = 0;
  #rendering = false;
  #scheduled = false;
  #loadingOlder = false;
  #hasMore = false;
  #oldestSeq = null;
  #botId = "";

  connectedCallback() {
    this.classList.add("log-wrap");
    if (!this.firstChild) this.#build();
    this.#log.addEventListener("scroll", this.#onScroll, { passive: true });
    this.#fab.addEventListener("click", this.pinToBottom);
    document.addEventListener(SERVER_STATE_EVENT, this.#onServerState);
  }

  disconnectedCallback() {
    this.#log.removeEventListener("scroll", this.#onScroll);
    this.#fab.removeEventListener("click", this.pinToBottom);
    document.removeEventListener(SERVER_STATE_EVENT, this.#onServerState);
  }

  #build() {
    this.#log = element("div");
    this.#log.id = "log";
    this.#status = element("div");
    this.#status.id = "status";
    this.#fab = element("button", "scroll-fab", "↓");
    this.#fab.id = "fab";
    this.#fab.type = "button";
    this.#fab.title = "Scroll to bottom";
    this.#badge = element("span", "n");
    this.#badge.id = "fab-n";
    this.#badge.hidden = true;
    this.#fab.append(this.#badge);
    this.append(this.#log, this.#status, this.#fab);
  }

  set botId(value) {
    this.#botId = value || "";
  }

  get botId() {
    return this.#botId;
  }

  get logElement() {
    return this.#log;
  }

  get statusElement() {
    return this.#status;
  }

  get pinned() {
    return this.#pinned;
  }

  get rendering() {
    return this.#rendering;
  }

  get itemCount() {
    return this.#items.length;
  }

  itemsFrom(index) {
    return this.#items.slice(index);
  }

  restore(items) {
    this.#items.push(...items);
  }

  setHistory({ hasMore, oldestSeq }) {
    this.#hasMore = Boolean(hasMore);
    this.#oldestSeq = oldestSeq ?? null;
  }

  get history() {
    return { hasMore: this.#hasMore, oldestSeq: this.#oldestSeq };
  }

  set loadingOlder(value) {
    this.#loadingOlder = Boolean(value);
  }

  get loadingOlder() {
    return this.#loadingOlder;
  }

  reset() {
    this.#items = [];
    this.#unread = 0;
    this.#hasMore = false;
    this.#oldestSeq = null;
    this.#pinned = true;
    this.#log.replaceChildren();
    this.#paintFab();
  }

  beginRender() {
    const snapshot = {
      height: this.#log.scrollHeight,
      top: this.#log.scrollTop,
      pinned: this.#pinned,
    };
    this.#items = [];
    this.#rendering = true;
    return snapshot;
  }

  finishRender(snapshot, keepScroll) {
    this.#rendering = false;
    this.flush();
    if (keepScroll && !snapshot.pinned) {
      this.#log.scrollTop = snapshot.top + this.#log.scrollHeight - snapshot.height;
    }
  }

  track(node, kind) {
    this.#items.push({ node, kind, height: 0 });
    if (!this.#pinned && !this.#rendering) this.#unread += 1;
    if (!this.#rendering) this.scheduleFlush();
    return node;
  }

  scheduleFlush() {
    if (this.#scheduled) return;
    this.#scheduled = true;
    requestAnimationFrame(() => {
      this.#scheduled = false;
      this.flush();
    });
  }

  flush() {
    const count = this.#items.length;
    const from = this.#pinned
      ? Math.max(0, count - PIN_WINDOW)
      : Math.max(0, Math.min(this.#visibleFrom(), Math.max(0, count - TAIL_ITEMS)));
    let topHeight = 0;
    for (let index = 0; index < from; index += 1) {
      topHeight += this.#items[index].height || ESTIMATED_ITEM_HEIGHT;
    }
    let spacer = this.#log.querySelector("[data-spacer='top']");
    if (!spacer) {
      spacer = element("div");
      spacer.dataset.spacer = "top";
      this.#log.prepend(spacer);
    }
    spacer.style.height = `${topHeight}px`;
    const keep = new Set([spacer]);
    for (let index = from; index < count; index += 1) keep.add(this.#items[index].node);
    let child = this.#log.firstElementChild;
    while (child) {
      const next = child.nextElementSibling;
      if (!keep.has(child)) child.remove();
      child = next;
    }
    let anchor = spacer;
    for (let index = from; index < count; index += 1) {
      const node = this.#items[index].node;
      if (node.parentNode !== this.#log || node.previousSibling !== anchor) anchor.after(node);
      anchor = node;
    }
    for (let index = from; index < count; index += 1) {
      const item = this.#items[index];
      item.height = item.node.offsetHeight || item.height || ESTIMATED_ITEM_HEIGHT;
    }
    this.#stickBottom();
    this.#paintFab();
  }

  nearBottom() {
    return this.#log.scrollHeight - this.#log.scrollTop - this.#log.clientHeight < PIN_WINDOW;
  }

  pinToBottom = () => {
    this.#pinned = true;
    this.#unread = 0;
    this.flush();
  };

  showStatus(value) {
    this.#status.replaceChildren();
    if (value instanceof Node) this.#status.append(value);
    else if (value) this.#status.textContent = String(value);
  }

  #visibleFrom() {
    let accumulated = 0;
    const top = this.#log.scrollTop - 1_200;
    for (const [index, item] of this.#items.entries()) {
      accumulated += item.height || ESTIMATED_ITEM_HEIGHT;
      if (accumulated >= top) return index;
    }
    return Math.max(0, this.#items.length - PIN_WINDOW);
  }

  #stickBottom() {
    if (!this.#pinned) return;
    this.#log.scrollTop = this.#log.scrollHeight;
    requestAnimationFrame(() => {
      if (this.#pinned) this.#log.scrollTop = this.#log.scrollHeight;
    });
  }

  #paintFab() {
    this.#fab.classList.toggle("on", !this.#pinned);
    this.#badge.hidden = this.#unread === 0;
    if (this.#unread) this.#badge.textContent = this.#unread > 99 ? "99+" : String(this.#unread);
  }
  #onScroll = () => {
    if (!this.#loadingOlder && this.#hasMore && this.#oldestSeq && this.#log.scrollTop < 64) {
      emitIntent(this, "load-older", { botId: this.#botId });
    }
    if (this.nearBottom()) {
      this.#pinned = true;
      this.#unread = 0;
      this.#paintFab();
    } else {
      this.#pinned = false;
      this.#paintFab();
      this.scheduleFlush();
    }
  };

  #onServerState = (event) => {
    const detail = event.detail;
    if (!detail || detail.ok !== false) return;
    if (!["attachment", "message-send", "bot-connection"].includes(detail.topic)) return;
    if (detail.owner?.botId && detail.owner.botId !== this.#botId) return;
    if (detail.error) this.showStatus(detail.error);
  };

  #log;
  #status;
  #fab;
  #badge;
}

customElements.define("reve-feed", ReveFeed);
