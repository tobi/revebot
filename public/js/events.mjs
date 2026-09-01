export const SERVER_STATE_EVENT = "reve:server-state";

let requestSequence = 0;

export function nextRequestId(scope = "request") {
  requestSequence += 1;
  return `${scope}:${requestSequence}`;
}

export function emitServerState(detail) {
  document.dispatchEvent(new CustomEvent(SERVER_STATE_EVENT, { detail }));
  return detail.data;
}

export function onServerState(listener, options) {
  document.addEventListener(SERVER_STATE_EVENT, listener, options);
  return () => document.removeEventListener(SERVER_STATE_EVENT, listener, options);
}

export function emitIntent(target, name, detail) {
  target.dispatchEvent(new CustomEvent(`reve:intent:${name}`, {
    bubbles: true,
    composed: true,
    detail,
  }));
}
