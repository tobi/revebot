function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function safeHttp(href) {
  try {
    const url = new URL(href);
    if (url.protocol === "http:" || url.protocol === "https:") return url.href;
  } catch (_) {}
  return null;
}

function appendInline(parent, source, fileNode) {
  const pattern = /<file\b[^>]*\/>|`[^`]+`|\*\*[^*]+?\*\*|~~[^~]+?~~|\+\+[^+]+?\+\+|\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)|https?:\/\/[^\s<]+|\*[^*\n]+?\*/g;
  let last = 0;
  let match;
  while ((match = pattern.exec(source))) {
    if (match.index > last) parent.appendChild(document.createTextNode(source.slice(last, match.index)));
    const token = match[0];
    if (token.startsWith("<file") && fileNode) {
      parent.appendChild(fileNode(token));
    } else if (token.startsWith("`")) {
      parent.appendChild(element("code", "chip", token.slice(1, -1)));
    } else if (token.startsWith("**")) {
      const node = document.createElement("strong");
      appendInline(node, token.slice(2, -2), fileNode);
      parent.appendChild(node);
    } else if (token.startsWith("~~")) {
      const node = document.createElement("del");
      appendInline(node, token.slice(2, -2), fileNode);
      parent.appendChild(node);
    } else if (token.startsWith("++")) {
      const node = document.createElement("u");
      appendInline(node, token.slice(2, -2), fileNode);
      parent.appendChild(node);
    } else if (token.startsWith("[")) {
      const href = safeHttp(match[2]);
      if (href) {
        const link = element("a", "", match[1]);
        link.href = href;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        parent.appendChild(link);
      } else {
        parent.appendChild(document.createTextNode(token));
      }
    } else if (token.startsWith("http")) {
      const trail = token.match(/[),.;!?]+$/);
      const raw = trail ? token.slice(0, -trail[0].length) : token;
      const href = safeHttp(raw);
      if (href) {
        const link = element("a", "", raw.replace(/^https?:\/\/(www\.)?/, "").replace(/\/$/, ""));
        link.href = href;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        parent.appendChild(link);
        if (trail) parent.appendChild(document.createTextNode(trail[0]));
      } else {
        parent.appendChild(document.createTextNode(token));
      }
    } else if (token.startsWith("*")) {
      const node = document.createElement("em");
      appendInline(node, token.slice(1, -1), fileNode);
      parent.appendChild(node);
    } else {
      parent.appendChild(document.createTextNode(token));
    }
    last = match.index + token.length;
  }
  if (last < source.length) parent.appendChild(document.createTextNode(source.slice(last)));
}

function formatText(source, options = {}) {
  const box = element("div", options.className || "bubble-content md");
  const fileNode = options.fileNode || null;
  const lines = String(source == null ? "" : source).replace(/\r\n/g, "\n").split("\n");
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.startsWith("```")) {
      const language = line.slice(3).trim().split(/\s+/)[0] || "";
      const body = [];
      index += 1;
      while (index < lines.length && !lines[index].startsWith("```")) {
        body.push(lines[index]);
        index += 1;
      }
      if (index < lines.length) index += 1;
      const fence = element("div", "fence");
      if (language) fence.appendChild(element("div", "fence-bar", language));
      const pre = element("pre");
      pre.appendChild(element("code", "", body.join("\n")));
      fence.appendChild(pre);
      box.appendChild(fence);
      continue;
    }
    const heading = line.match(/^(#{1,3})\s+(.+)/);
    if (heading) {
      const node = document.createElement("h" + heading[1].length);
      appendInline(node, heading[2], fileNode);
      box.appendChild(node);
      index += 1;
      continue;
    }
    if (/^>\s?/.test(line)) {
      const quote = document.createElement("blockquote");
      const body = [];
      while (index < lines.length && /^>\s?/.test(lines[index])) {
        body.push(lines[index].replace(/^>\s?/, ""));
        index += 1;
      }
      body.forEach((value, lineIndex) => {
        if (lineIndex) quote.appendChild(document.createElement("br"));
        appendInline(quote, value, fileNode);
      });
      box.appendChild(quote);
      continue;
    }
    const unordered = /^\s*[-*]\s+/.test(line);
    const ordered = /^\s*\d+\.\s+/.test(line);
    if (unordered || ordered) {
      const list = document.createElement(ordered ? "ol" : "ul");
      const item = ordered ? /^\s*\d+\.\s+/ : /^\s*[-*]\s+/;
      while (index < lines.length && item.test(lines[index])) {
        const listItem = document.createElement("li");
        appendInline(listItem, lines[index].replace(item, ""), fileNode);
        list.appendChild(listItem);
        index += 1;
      }
      box.appendChild(list);
      continue;
    }
    if (line.trim() === "") {
      index += 1;
      continue;
    }
    const paragraphLines = [line];
    index += 1;
    while (index < lines.length && lines[index].trim() !== ""
      && !lines[index].startsWith("```") && !/^#{1,3}\s+/.test(lines[index])
      && !/^\s*[-*]\s+/.test(lines[index]) && !/^\s*\d+\.\s+/.test(lines[index])
      && !/^>\s?/.test(lines[index])) {
      paragraphLines.push(lines[index]);
      index += 1;
    }
    const paragraph = document.createElement("p");
    paragraphLines.forEach((value, lineIndex) => {
      if (lineIndex) paragraph.appendChild(document.createElement("br"));
      appendInline(paragraph, value, fileNode);
    });
    box.appendChild(paragraph);
  }
  return box;
}

export { formatText };
