import { formatText } from "../lib/markdown.mjs";
import { emitIntent, nextRequestId, SERVER_STATE_EVENT } from "../events.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function extension(name) {
  const match = String(name || "").match(/\.([^.]+)$/);
  return match ? match[1].slice(0, 5).toUpperCase() : "FILE";
}

function formatBytes(value) {
  const bytes = Number(value) || 0;
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(bytes < 10 * 1024 * 1024 ? 1 : 0)} MB`;
}

function kindOf(attachment) {
  const mime = String(attachment.mime || "").toLowerCase();
  const name = String(attachment.name || "").toLowerCase();
  if (mime.startsWith("image/")) return "image";
  if (mime === "text/html" || name.endsWith(".html") || name.endsWith(".htm")) return "html";
  if (mime === "text/markdown" || name.endsWith(".md") || name.endsWith(".markdown")) return "markdown";
  return "file";
}

function attachmentUrl(attachment, botId, token) {
  if (attachment.url) {
    const join = attachment.url.includes("?") ? "&" : "?";
    return attachment.url + join + "token=" + encodeURIComponent(token);
  }
  if (!attachment.id || !attachment.name || !botId) return "";
  return "/api/bots/" + encodeURIComponent(botId) + "/attachments/" +
    encodeURIComponent(attachment.id) + "/" + encodeURIComponent(attachment.name) +
    "?token=" + encodeURIComponent(token);
}

export class ReveAttachment extends HTMLElement {
  #attachment = null;
  #botId = "";
  #token = "";
  #textContent = null;
  #requestId = "";
  #previewBody = null;
  #previewKind = "";

  set data(value) {
    const next = value?.attachment || null;
    if (next !== this.#attachment) {
      this.#textContent = null;
      this.#requestId = "";
    }
    this.#attachment = next;
    this.#botId = value?.botId || "";
    this.#token = value?.token || "";
    if (this.isConnected) this.#render();
  }

  connectedCallback() {
    document.addEventListener(SERVER_STATE_EVENT, this.#onServerState);
    this.#render();
  }

  disconnectedCallback() {
    document.removeEventListener(SERVER_STATE_EVENT, this.#onServerState);
  }

  #onServerState = (event) => {
    const detail = event.detail;
    if (detail.topic !== "attachment-preview" || detail.requestId !== this.#requestId) return;
    if (detail.owner?.botId !== this.#botId || !this.#previewBody) return;
    this.#requestId = "";
    if (!detail.ok || typeof detail.data?.text !== "string") {
      this.#previewBody.replaceChildren(element(
        "div",
        "attachment-preview-error",
        detail.error || "Preview failed",
      ));
      return;
    }
    this.#textContent = detail.data.text;
    this.#paintTextPreview();
  };

  #render() {
    const attachment = this.#attachment;
    if (!attachment) return;
    const kind = kindOf(attachment);
    const url = attachmentUrl(attachment, this.#botId, this.#token);
    const card = element("article", `attachment-card ${kind}`);

    if (kind === "image" && url) {
      const preview = element("button", "attachment-image");
      preview.type = "button";
      preview.setAttribute("aria-label", `Preview ${attachment.name}`);
      const image = document.createElement("img");
      image.src = url;
      image.alt = attachment.name || "Image attachment";
      image.loading = "lazy";
      preview.appendChild(image);
      preview.addEventListener("click", () => this.#open(kind, url));
      card.appendChild(preview);
    } else {
      card.appendChild(element("div", "attachment-type", extension(attachment.name)));
    }

    const detail = element("div", "attachment-detail");
    detail.appendChild(element("div", "attachment-name", attachment.name || attachment.path || "Attachment"));
    detail.appendChild(element(
      "div",
      "attachment-meta",
      [attachment.mime || "File", formatBytes(attachment.bytes)].filter(Boolean).join(" · "),
    ));
    card.appendChild(detail);

    const actions = element("div", "attachment-actions");
    if (url && kind !== "file") {
      const open = element("button", "attachment-action", kind === "image" ? "View" : "Open");
      open.type = "button";
      open.addEventListener("click", () => this.#open(kind, url));
      actions.appendChild(open);
    }
    const download = element("a", "attachment-action download", "Download");
    download.href = url || "#";
    download.download = attachment.name || "attachment";
    if (!url) download.setAttribute("aria-disabled", "true");
    actions.appendChild(download);
    card.appendChild(actions);

    const dialog = element("dialog", "attachment-lightbox");
    dialog.addEventListener("click", (event) => {
      if (event.target === dialog) dialog.close?.();
    });
    this.replaceChildren(card, dialog);
  }

  #paintTextPreview() {
    if (!this.#previewBody || this.#textContent == null) return;
    if (this.#previewKind === "html") {
      const frame = document.createElement("iframe");
      frame.title = this.#attachment?.name || "HTML attachment";
      frame.setAttribute("sandbox", "");
      frame.srcdoc = this.#textContent;
      this.#previewBody.replaceChildren(frame);
    } else {
      this.#previewBody.replaceChildren(formatText(this.#textContent, {
        className: "attachment-markdown md",
      }));
    }
  }

  #open(kind, url) {
    const attachment = this.#attachment;
    const dialog = this.querySelector(".attachment-lightbox");
    if (!attachment || !dialog) return;
    const shell = element("div", "attachment-lightbox-shell");
    const bar = element("div", "attachment-lightbox-bar");
    bar.appendChild(element("strong", "attachment-lightbox-title", attachment.name || "Attachment"));
    const download = element("a", "attachment-action download", "Download");
    download.href = url;
    download.download = attachment.name || "attachment";
    bar.appendChild(download);
    const close = element("button", "attachment-lightbox-close", "Close");
    close.type = "button";
    close.addEventListener("click", () => dialog.close?.());
    bar.appendChild(close);
    const body = element("div", `attachment-lightbox-body ${kind}`);
    shell.append(bar, body);
    dialog.replaceChildren(shell);
    if (!dialog.open) {
      if (typeof dialog.showModal === "function") dialog.showModal();
      else dialog.setAttribute("open", "");
    }

    if (kind === "image") {
      const image = document.createElement("img");
      image.src = url;
      image.alt = attachment.name || "Image attachment";
      body.appendChild(image);
      return;
    }
    this.#previewBody = body;
    this.#previewKind = kind;
    if (this.#textContent != null) {
      this.#paintTextPreview();
      return;
    }
    body.appendChild(element("div", "attachment-loading", "Loading preview…"));
    this.#requestId = nextRequestId("attachment-preview");
    emitIntent(this, "attachment-preview", {
      requestId: this.#requestId,
      botId: this.#botId,
      attachment: {
        id: attachment.id || "",
        name: attachment.name || "",
        url: attachment.url || "",
      },
    });
  }
}

customElements.define("reve-attachment", ReveAttachment);

export { attachmentUrl, formatBytes, kindOf };
