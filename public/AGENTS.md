# Reve browser surface

`public/` contains authored browser assets embedded in the `revebot` binary. Treat every change as performance-sensitive browser code. Keep the surface usable without npm, a CDN, or a runtime asset directory.

## Layout and build

- Put executable browser code under `public/js/`; do not add executable inline JavaScript to HTML. An inert configuration element containing the per-process token is allowed.
- Use native ES modules and Custom Elements. Do not add Lit, another framework, a package manager, or a bundler unless the component model genuinely needs it and the repository adopts that build dependency deliberately.
- `build.rs` owns the embedded asset table and asset manifest. Do not hand-maintain parallel Rust route lists.
- Source files are served untransformed, so browser stack traces already name the authored files and lines. Do not generate fake identity source maps. A future transform must emit real source maps and preserve the no-network build.
- Do not serve this `AGENTS.md` file as a public asset.

## State and components

- Keep view ownership explicit. `<reve-feed>`, `<reve-composer>`, `<reve-autocomplete>`, and `<reve-attachment>` own their DOM and mutable UI state. Attachment previews emit correlated transport intents; the entrypoint fetches bytes and publishes the result through `reve:server-state`.
- Components communicate with bubbling, composed DOM events. They must not reach into another component's private DOM or share mutable module globals.
- Intent events use the `reve:intent:*` namespace. Server-derived state uses `reve:server-state` with the stable envelope from `js/events.js`.
- Every HTTP response body and WebSocket frame that changes UI state must pass through the server-state event bridge. Components subscribe to current server state instead of retaining a startup snapshot.
- Include request identity plus relevant bot/conversation identity in asynchronous event details. Abort obsolete fetches and ignore late state that no longer matches the active request and owner.
- Never place the bearer token, request Authorization header, pasted secret value, or secret-source output in a DOM event. Emit server responses, not sensitive request bodies.
- Keep rendering deterministic: state event in, DOM state out. Network effects belong in the entrypoint/transport layer; components emit intents.

## Performance invariants

- One autocomplete request per menu invocation, not per keystroke. Filter the returned catalog locally until that invocation closes.
- Do not stringify, deep-clone, or redispatch copies of server payloads. Event detail is borrowed immutable state; consumers must not mutate it.
- Reconcile keyed nodes. Do not clear and rebuild an unchanged feed or list.
- Batch related DOM writes. Use `DocumentFragment` for multi-node insertion and `requestAnimationFrame` only when work benefits from frame coalescing.
- Separate layout reads from writes. Never alternate `getBoundingClientRect`, `scrollHeight`, or computed-style reads with DOM mutations in a loop.
- Use event delegation for repeated rows and passive listeners for non-cancelled pointer/scroll observation. Do not allocate a closure per stable list row when one delegated handler suffices.
- Keep the initial module graph small and acyclic. Avoid dynamic imports on typing or scrolling paths.

## Verification

- Add a behavior test for each event contract and component transition. Exercise stale-response rejection, disconnect/reconnect, and bot switching where applicable.
- Parse and execute the actual served module graph in tests. Source-text assertions alone are not behavioral coverage.
- For UI changes, launch the real page and inspect interaction plus browser console state; unit tests are not visual verification.
