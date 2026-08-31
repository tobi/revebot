-- The sandbox every command runs in.
--
-- workspace/ is mounted at /workspace and is the working directory, so a
-- relative path means the same thing on the host and in the VM. The agent's
-- own definition files stay outside it.
--
-- The microVM is mandatory: Reve links the microsandbox Rust crate directly
-- and refuses to run without it. There is no host or local mode.
--
-- The guest can reach the public internet. Set `open = false` and list
-- hosts in `allow` to lock down to an allowlist.
--
-- The default image is wrap:desktop: toolchain (rust, go, node, bun, pnpm,
-- python, mise), unprivileged `user`, XFCE on :1, VNC/noVNC, and a shared
-- Chrome that agent-browser attaches to. Point `image` at a bare distro
-- instead and set `provision = true` to get install-on-first-boot back.

sandbox {
  image = "ghcr.io/tobi/wrap:desktop",
  cpus = 2,
  memory = 8192,
  -- The writable rootfs layer, in MiB. A real build tree needs room.
  root_disk = 16384,

  -- Public internet. Flip to false and fill `allow` to lock down.
  open = true,
  -- allow = { "github.com", "api.github.com" },

  -- A credential the VM may use without ever holding it: the guest sees only
  -- the placeholder and the proxy substitutes the real value for these hosts.
  -- `source` is a host env var, or `$(command)` run on the host at boot.
  -- `$(gh auth token)` reads the OS keyring; export GITHUB_TOKEN=... also works.
  --   export OPENROUTER_API_KEY=...
  secrets = {
    {
      env = "GITHUB_TOKEN",
      source = "$(gh auth token)",
      placeholder = "reve-github-token",
      hosts = {
        ["github.com"] = { allow = true },
        ["api.github.com"] = { allow = true },
      },
    },
    {
      env = "OPENROUTER_API_KEY",
      source = "OPENROUTER_API_KEY",
      placeholder = "reve-openrouter-key",
      hosts = {
        ["openrouter.ai"] = { allow = true },
      },
    },
  },

  -- bootstrap = { "npm ci" },
}
