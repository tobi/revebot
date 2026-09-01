export const CONVERSATION_COMMANDS = Object.freeze([
  Object.freeze({
    name: "compact",
    description: "Compact this conversation; optional text guides the summary",
    acceptsArguments: true,
    source: "command",
  }),
  Object.freeze({
    name: "new",
    description: "Start a fresh conversation for this bot",
    acceptsArguments: false,
    source: "command",
  }),
  Object.freeze({
    name: "fork",
    description: "Fork this conversation into a new browser tab",
    acceptsArguments: false,
    source: "command",
  }),
]);

const COMMAND_NAMES = new Set(CONVERSATION_COMMANDS.map((command) => command.name));

export function parseConversationCommand(text, attachmentCount) {
  if (attachmentCount) return null;
  const match = /^\/([a-z0-9_-]+)(?:\s+([\s\S]*))?$/.exec((text || "").trim());
  if (!match || !COMMAND_NAMES.has(match[1])) return null;
  return {
    command: match[1],
    instructions: match[1] === "compact" ? (match[2] || "").trim() || null : null,
    invalidArguments: match[1] !== "compact" && Boolean((match[2] || "").trim()),
  };
}
