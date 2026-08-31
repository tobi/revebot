# Security policy

Reve executes model-authored code, so sandbox boundary failures and credential disclosure
are security issues, not ordinary bugs.

## Supported versions

Security fixes are provided for the latest released version. Until a stable 1.0 release,
users should update to the newest 0.x release rather than expecting fixes on older minors.

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability. Use GitHub's private
security advisory form:

<https://github.com/tobi/reve/security/advisories/new>

Include the Reve version (`reve --version`), operating system, the pinned `microsandbox`
crate version (`=0.6.8`, the only sandbox dependency, declared in `Cargo.toml`), reproduction
steps, and potential impact. Remove API keys, model transcripts, session contents, and
other private workspace data from reports.

Reports involving a host command escape, workspace bind escape, unscoped secret exposure,
network-policy bypass, durable-record corruption, or recovery replay of an effectful tool
will be treated as high priority. You can expect acknowledgment within seven days and a
status update after the report has been reproduced.

## Security model

Reve deliberately fails closed:

- Every command a tool issues — `ctx.sh` — and every `reve exec` runs in the same mandatory
  microsandbox microVM. There is no host-shell, local, CLI, or FFI fallback.
- The sandbox is provided exclusively by the `microsandbox` Rust crate, pinned `=0.6.8` in
  `Cargo.toml`. It is Reve's only sandbox dependency, linked and called directly — no FFI
  shim, no daemon, no CLI transport.
- Only `workspace/` is bind-mounted into the VM, at `/workspace`, and set as the working
  directory. Host configuration and installed host tools stay outside the mount;
  bot profiles, instructions and workspace Lua are editable inside it.
- Network access is the public internet by default (`NetworkProfile::Public` plus
  gateway DNS). `sandbox.lua` can set `open = false` and list hosts in `allow` to
  lock down; that path starts from `NetworkPolicy::none()`. Private/LAN and cloud
  metadata are not included in the default.
- Secrets are scoped per host. Configuration stores a host environment variable name, not
  its value. Microsandbox resolves that source when the VM starts; the guest sees only the
  placeholder, and the real value is injected into requests to named hosts at the network
  boundary. An unscoped secret is refused. Removed secrets are deleted from reused VM
  definitions before restart, and runtime secret changes never enter the disk fingerprint.
- Durable intent records are written before effects so recovery does not guess whether an
  effectful operation should be replayed.

The host Rust process, host-installed Lua (`agent.lua`, `sandbox.lua`, `tools/*.lua`,
`plugins/*.lua`), configured model providers, and the upstream microsandbox runtime
remain trusted. Host Lua must not import bot-provided source or expose privileged
functions to it. There is no host command path in either Lua state.

Bot-editable workspace plugins/routines execute in a **separate restricted Lua
state**. Its allowlisted pure libraries exclude ambient host filesystem, environment,
module/native loading and stdio access. Registry values are tied to their originating
state. Workspace source is read beneath the house with descriptor-relative
`O_NOFOLLOW` opens; symlinked files/ancestors, non-regular files, oversized source
(> 1 MiB) and bytecode are refused. `ctx.sh` executes only in the guest.

This restricts capabilities, not resources: pure Lua is not preemptively cancelled
or CPU/memory-budgeted. It is not a hostile-code process sandbox; substantial
computation belongs in the VM. Other shared-workspace risks (including bot-authored
profile paths and writable session files) are separate from this Lua boundary.
The complete supported API and current limits are in the `plugins` skill.
