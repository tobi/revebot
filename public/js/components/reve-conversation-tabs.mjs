import { emitIntent } from "../events.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

export class ReveConversationTabs extends HTMLElement {
  #tabs = [];
  #selected = "";
  #nodes = new Map();

  connectedCallback() {
    this.classList.add("conversation-tabs");
    this.setAttribute("role", "tablist");
    this.setAttribute("aria-label", "Conversations");
    this.addEventListener("click", this.#onClick);
    this.addEventListener("keydown", this.#onKeydown);
    this.#paint();
  }

  disconnectedCallback() {
    this.removeEventListener("click", this.#onClick);
    this.removeEventListener("keydown", this.#onKeydown);
  }

  set state(value) {
    this.#tabs = Array.isArray(value?.tabs)
      ? value.tabs.filter((tab) => typeof tab?.id === "string" && tab.id)
      : [];
    this.#selected = typeof value?.selected === "string" ? value.selected : "";
    this.#paint();
  }

  get state() {
    return { tabs: this.#tabs, selected: this.#selected };
  }

  select(conversationId) {
    if (!this.#tabs.some((tab) => tab.id === conversationId)) return;
    emitIntent(this, "select-conversation", { conversationId });
  }

  #paint() {
    const keep = new Set();
    const fragment = document.createDocumentFragment();
    for (const tab of this.#tabs) {
      let button = this.#nodes.get(tab.id);
      if (!button) {
        button = element("button", "conversation-tab");
        button.type = "button";
        button.setAttribute("role", "tab");
        button.dataset.conversationId = tab.id;
        this.#nodes.set(tab.id, button);
      }
      const selected = tab.id === this.#selected;
      button.classList.toggle("on", selected);
      button.setAttribute("aria-selected", selected ? "true" : "false");
      button.tabIndex = selected ? 0 : -1;
      button.title = tab.readOnly ? `${tab.label} · read-only` : tab.label;
      button.replaceChildren(
        element("span", "conversation-tab-label", tab.label),
        element("span", `conversation-tab-state${tab.readOnly ? " read-only" : ""}`, tab.readOnly ? "history" : "live"),
      );
      keep.add(tab.id);
      fragment.appendChild(button);
    }
    for (const id of this.#nodes.keys()) {
      if (!keep.has(id)) this.#nodes.delete(id);
    }
    this.replaceChildren(fragment);
    this.hidden = this.#tabs.length === 0;
  }

  #onClick = (event) => {
    const button = event.target.closest?.("[data-conversation-id]");
    if (button) this.select(button.dataset.conversationId);
  };

  #onKeydown = (event) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    const buttons = [...this.querySelectorAll("[data-conversation-id]")];
    if (!buttons.length) return;
    const current = Math.max(0, buttons.indexOf(event.target));
    const index = event.key === "Home"
      ? 0
      : event.key === "End"
        ? buttons.length - 1
        : (current + (event.key === "ArrowRight" ? 1 : -1) + buttons.length) % buttons.length;
    event.preventDefault();
    buttons[index].focus();
    this.select(buttons[index].dataset.conversationId);
  };
}

customElements.define("reve-conversation-tabs", ReveConversationTabs);
