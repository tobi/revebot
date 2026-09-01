# Contributing to revebot

Thanks for helping improve revebot.

## Development setup

Revebot requires Rust 1.91 or newer, on Linux with KVM or macOS on Apple Silicon
(the microsandbox runtime needs one or the other).

```bash
git clone https://github.com/tobi/revebot.git
cd revebot
cargo fetch --locked
cargo build
make ci
```

The default test suite must not make model requests, provision a VM, or depend on
a developer's global configuration. Real-microVM tests are opt-in and `#[ignore]`d:

```bash
cargo test --locked --test microvm -- --ignored
```

Before opening a pull request, run `make ci`. If your change touches the sandbox,
also run the microVM tests.

## Design constraints

Please read `AGENTS.md`, `docs/harness.md` (the canonical durable-harness
specification), and `docs/architecture.md` (how it maps onto this crate) before
changing architecture. In particular:

- Revebot has no host-shell, local-sandbox, CLI, or FFI fallback. If the microVM
  cannot boot, startup fails closed.
- There is exactly one sandbox transport: the `microsandbox` crate, linked
  directly. Do not add a second transport or a host-shell path — not even for
  tests, diagnostics, or convenience.
- No host command path is exposed to Lua. A tool's body runs on the host, but
  `ctx.sh` is its only command path and it goes to the microVM. Do not add a
  `ctx.host_exec` or equivalent.
- `Storage` is single-owner by construction: it is not thread-safe and must not
  be shared behind a mutex. Serialize access through the owning task.
- Record an effect's intent and result identifiers before performing the effect;
  ids are provisioned before the effect they name.
- Never silently overwrite files a user has edited in a house directory.
  `revebot init` is idempotent and keeps edited files.
- Add a focused test for every behavior change. Keep `make ci` green.

## Pull requests

Keep changes focused and explain their durability and sandbox implications.
Include the commands used to verify the change. Do not include API keys, durable
session files, VM images, or user workspace contents.
