import { emitIntent, nextRequestId, SERVER_STATE_EVENT } from "../events.mjs";

const LARGE_PASTE_BYTES = 8_000;
const READY_PLACEHOLDER = "Message  ·  @ bots, / commands";

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text) element.textContent = text;
  return element;
}

export class ReveComposer extends HTMLElement {
  #botId = "";
  #autocomplete = null;
  #drafts = new Map();
  #attachments = new Map();
  #uploads = [];
  #upload = null;
  #send = null;
  #statusline = null;

  connectedCallback() {
    this.classList.add("compose");
    if (!this.firstChild) this.#build();
    this.#form.addEventListener("submit", this.#onSubmit);
    this.#file.addEventListener("change", this.#onFiles);
    this.#camera.addEventListener("change", this.#onFiles);
    this.#input.addEventListener("paste", this.#onPaste);
    this.#attachmentRow.addEventListener("click", this.#onAttachmentClick);
    window.addEventListener("dragenter", this.#onDrag, { passive: false });
    window.addEventListener("dragover", this.#onDrag, { passive: false });
    window.addEventListener("dragleave", this.#onDragLeave, { passive: true });
    window.addEventListener("drop", this.#onDrop, { passive: false });
    document.addEventListener(SERVER_STATE_EVENT, this.#onServerState);
  }

  disconnectedCallback() {
    this.#form.removeEventListener("submit", this.#onSubmit);
    this.#file.removeEventListener("change", this.#onFiles);
    this.#camera.removeEventListener("change", this.#onFiles);
    this.#input.removeEventListener("paste", this.#onPaste);
    this.#attachmentRow.removeEventListener("click", this.#onAttachmentClick);
    window.removeEventListener("dragenter", this.#onDrag);
    window.removeEventListener("dragover", this.#onDrag);
    window.removeEventListener("dragleave", this.#onDragLeave);
    window.removeEventListener("drop", this.#onDrop);
    document.removeEventListener(SERVER_STATE_EVENT, this.#onServerState);
    this.#upload?.abort.abort();
  }

  #build() {
    const attachmentRow = node("div", "attach-row");
    attachmentRow.id = "attach-row";
    const form = document.createElement("form");
    const box = node("div", "box");
    const create = node("button", "icon", "+");
    create.id = "new2";
    create.type = "button";
    create.title = "New agent";
    create.addEventListener("click", () => emitIntent(this, "new-bot", {}));
    const file = document.createElement("input");
    file.id = "file";
    file.type = "file";
    file.multiple = true;
    file.hidden = true;
    const fileLabel = node("label", "icon");
    fileLabel.htmlFor = file.id;
    fileLabel.title = "Attach";
    fileLabel.innerHTML = '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M21.4 11.4 12 20.8a6 6 0 0 1-8.5-8.5l9.5-9.5a4 4 0 0 1 5.6 5.6L9 18.4a2 2 0 1 1-2.8-2.8l8.5-8.5"/></svg>';
    const camera = document.createElement("input");
    camera.id = "camera";
    camera.type = "file";
    camera.accept = "image/*";
    camera.capture = "environment";
    camera.hidden = true;
    const cameraLabel = node("label", "icon camera-btn");
    cameraLabel.htmlFor = camera.id;
    cameraLabel.title = "Photo";
    cameraLabel.innerHTML = '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M4 8h3l2-3h6l2 3h3v12H4z"/><circle cx="12" cy="13" r="4"/></svg>';
    const input = document.createElement("textarea");
    input.id = "text";
    input.placeholder = READY_PLACEHOLDER;
    input.rows = 1;
    input.autocomplete = "off";
    input.autocapitalize = "sentences";
    input.enterKeyHint = "send";
    input.addEventListener("keydown", this.#onKeyDown);
    const submit = node("button", "send", "↑");
    submit.type = "submit";
    submit.title = "Send";
    box.append(create, fileLabel, cameraLabel, file, camera, input, submit);
    form.append(box);
    this.#attachmentRow = attachmentRow;
    this.#form = form;
    this.#file = file;
    this.#camera = camera;
    this.#input = input;
    this.#submit = submit;
    const statusline = node("div", "statusline");
    statusline.hidden = true;
    this.#statusline = statusline;
    this.append(attachmentRow, form, statusline);
  }

  setStatusline(text) {
    if (!this.#statusline) return;
    const line = (text || "").trim();
    this.#statusline.textContent = line;
    this.#statusline.hidden = !line;
  }

  set autocomplete(component) {
    this.#autocomplete = component;
    component?.attach(this.#input);
    if (component) component.botId = this.#botId;
  }

  set botId(value) {
    const next = value || "";
    if (next === this.#botId) return;
    if (this.#botId) this.#drafts.set(this.#botId, this.#input.value);
    this.#botId = next;
    this.#input.value = this.#drafts.get(next) || "";
    this.#autocomplete?.close();
    if (this.#autocomplete) this.#autocomplete.botId = next;
    this.#paintAttachments();
  }

  get botId() {
    return this.#botId;
  }

  get value() {
    return this.#input.value;
  }

  set value(text) {
    this.#input.value = text || "";
  }

  set readOnly(value) {
    const readOnly = Boolean(value);
    this.#input.readOnly = readOnly;
    this.#file.disabled = readOnly;
    this.#camera.disabled = readOnly;
    this.#submit.disabled = readOnly;
    this.#input.placeholder = readOnly
      ? "Fork opened in another tab · this conversation is read-only"
      : READY_PLACEHOLDER;
  }

  get readOnly() {
    return this.#input.readOnly;
  }

  get input() {
    return this.#input;
  }

  focus() {
    this.#input.focus();
  }

  submissionPayload(requestId) {
    if (this.#send?.requestId !== requestId) return null;
    return {
      text: this.#send.body,
      rawText: this.#send.text,
      attachmentCount: this.#send.attachments.length,
    };
  }

  attachmentPayload(requestId) {
    if (this.#upload?.requestId !== requestId) return null;
    return { file: this.#upload.file, signal: this.#upload.abort.signal };
  }

  #getAttachments(botId = this.#botId) {
    let attachments = this.#attachments.get(botId);
    if (!attachments) {
      attachments = [];
      this.#attachments.set(botId, attachments);
    }
    return attachments;
  }

  #onSubmit = (event) => {
    event.preventDefault();
    if (this.#autocomplete?.acceptIfAvailable()) return;
    this.#autocomplete?.close();
    if (!this.#botId || this.#send) return;
    const text = this.#input.value.trim();
    const attachments = this.#getAttachments();
    const tags = attachments.map((attachment) => attachment.tag).filter(Boolean);
    if (!text && !tags.length) return;
    const body = [text, ...tags].filter(Boolean).join("\n");
    const requestId = nextRequestId("send");
    this.#send = { requestId, botId: this.#botId, text, body, attachments };
    this.#input.value = "";
    this.#attachments.set(this.#botId, []);
    this.#drafts.delete(this.#botId);
    this.#paintAttachments();
    emitIntent(this, "send", { requestId, botId: this.#botId });
  };

  #onServerState = (event) => {
    const detail = event.detail;
    if (!detail) return;
    if (detail.topic === "attachment" && this.#upload?.requestId === detail.requestId) {
      this.#finishUpload(detail);
    } else if (detail.topic === "message-send" && this.#send?.requestId === detail.requestId) {
      this.#finishSend(detail);
    }
  };

  #finishSend(detail) {
    const sent = this.#send;
    this.#send = null;
    if (!sent) return;
    if (detail.ok) {
      for (const attachment of sent.attachments) {
        if (attachment.preview) URL.revokeObjectURL(attachment.preview);
      }
      return;
    }
    const currentDraft = this.#drafts.get(sent.botId) || (this.#botId === sent.botId ? this.#input.value : "");
    const restored = [sent.text, currentDraft].filter(Boolean).join("\n");
    this.#drafts.set(sent.botId, restored);
    const currentAttachments = this.#getAttachments(sent.botId);
    const currentIds = new Set(currentAttachments.map((attachment) => attachment.id));
    this.#attachments.set(
      sent.botId,
      sent.attachments.filter((attachment) => !currentIds.has(attachment.id)).concat(currentAttachments),
    );
    if (this.#botId === sent.botId) {
      this.#input.value = restored;
      this.#paintAttachments();
    }
  }

  queueFiles(files) {
    for (const file of files) {
      if (file) this.#uploads.push({ botId: this.#botId, file });
    }
    this.#pumpUploads();
  }

  #pumpUploads() {
    if (this.#upload || !this.#uploads.length) return;
    const next = this.#uploads.shift();
    if (!next?.botId) {
      this.#pumpUploads();
      return;
    }
    const requestId = nextRequestId("attachment");
    const abort = new AbortController();
    this.#upload = { ...next, requestId, abort };
    emitIntent(this, "attachment", { requestId, botId: next.botId });
  }

  #finishUpload(detail) {
    const upload = this.#upload;
    this.#upload = null;
    if (detail.ok && upload) {
      const saved = detail.data;
      const preview = upload.file.type?.startsWith("image/") ? URL.createObjectURL(upload.file) : "";
      this.#getAttachments(upload.botId).push({ ...saved, preview });
      if (this.#botId === upload.botId) this.#paintAttachments();
    }
    this.#pumpUploads();
  }

  #paintAttachments() {
    const fragment = document.createDocumentFragment();
    const attachments = this.#getAttachments();
    for (const attachment of attachments) {
      const chip = node("div", "att");
      chip.dataset.id = attachment.id || "";
      if (attachment.preview) {
        const image = document.createElement("img");
        image.alt = "";
        image.src = attachment.preview;
        chip.append(image);
      } else {
        chip.append(node("span", "att-ic", attachment.mime?.startsWith("image/") ? "image" : "file"));
      }
      chip.append(node("span", "att-name", attachment.name || "file"));
      const remove = node("button", "att-x", "×");
      remove.type = "button";
      remove.dataset.remove = attachment.id || "";
      chip.append(remove);
      fragment.append(chip);
    }
    this.#attachmentRow.replaceChildren(fragment);
  }

  #onAttachmentClick = (event) => {
    const button = event.target.closest("[data-remove]");
    if (button) this.#removeAttachment(button.dataset.remove);
  };

  #removeAttachment(id) {
    const attachments = this.#getAttachments();
    const index = attachments.findIndex((attachment) => attachment.id === id);
    if (index < 0) return;
    const [removed] = attachments.splice(index, 1);
    if (removed?.preview) URL.revokeObjectURL(removed.preview);
    this.#paintAttachments();
  }

  #onFiles = (event) => {
    this.queueFiles(event.target.files || []);
    event.target.value = "";
  };

  #onPaste = (event) => {
    const clipboard = event.clipboardData;
    if (!clipboard) return;
    if (clipboard.files.length) {
      event.preventDefault();
      this.queueFiles(clipboard.files);
      return;
    }
    const pasted = clipboard.getData("text/plain") || "";
    if (pasted.length <= LARGE_PASTE_BYTES) return;
    event.preventDefault();
    this.queueFiles([new File([pasted], "paste.txt", { type: "text/plain" })]);
  };

  #onDrag = (event) => {
    if (!event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    document.body.classList.add("dropping");
  };

  #onDragLeave = (event) => {
    if (!event.relatedTarget) document.body.classList.remove("dropping");
  };

  #onDrop = (event) => {
    document.body.classList.remove("dropping");
    if (!event.dataTransfer?.files.length) return;
    event.preventDefault();
    this.queueFiles(event.dataTransfer.files);
  };

  #onKeyDown = (event) => {
    if (event.defaultPrevented || event.isComposing) return;
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      this.#form.requestSubmit();
    }
  };

  #attachmentRow;
  #form;
  #file;
  #camera;
  #input;
  #submit;


}
customElements.define("reve-composer", ReveComposer);
