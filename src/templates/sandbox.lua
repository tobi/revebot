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
-- The default image already contains the toolchain -- rust, go, node, bun,
-- pnpm, python, and mise to add more -- so there is no provisioning step and
-- the first command runs as soon as the VM boots. Point `image` at a bare
-- distro instead and set `provision = true` to get the install-on-first-boot
-- behaviour back.

sandbox {
  image = "ghcr.io/tobi/wrap:latest",
  cpus = 2,
  memory = 2048,
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
      hosts = { "github.com", "api.github.com" },
    },
    {
      env = "OPENROUTER_API_KEY",
      source = "OPENROUTER_API_KEY",
      placeholder = "reve-openrouter-key",
      hosts = { "openrouter.ai" },
    },
  },

  -- bootstrap = { "npm ci" },
}
