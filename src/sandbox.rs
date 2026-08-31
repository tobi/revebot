//! The mandatory microVM.
//!
//! Reve links the `microsandbox` crate directly. There is no CLI, no daemon, no
//! FFI shim, and no host-shell path: if the VM cannot boot, the agent refuses
//! to run rather than quietly executing model-authored commands on your machine.
//!
//! Egress is open to the public internet by default. The guest can reach
//! [`NetworkProfile::Public`] (plus the gateway-DNS rule names need). Set
//! `open = false` in `sandbox.lua` and list hosts in `allow` to lock down to
//! an allowlist; that path still starts from [`NetworkPolicy::none`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use microsandbox::sandbox::StatVirtualization;
use microsandbox::size::SizeExt;
use microsandbox::{Sandbox as MsbSandbox, SandboxModificationBuilder, SecretSource};
use microsandbox_network::policy::{NetworkPolicy, NetworkProfile, Rule};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("the microVM is unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox operation failed: {0}")]
    Failed(String),
    #[error("invalid sandbox policy: {0}")]
    Policy(String),
}

pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

/// The default guest: wrap's desktop image. Toolchain at absolute paths under
/// `/opt`, unprivileged `user` (`HOME=/home/user`), and a headless XFCE desktop
/// with VNC/noVNC plus a shared Chrome that `agent-browser` attaches to.
///
/// Baking the toolchain into the image rather than installing it on first boot
/// is the difference between a cold start of seconds and one of minutes, and it
/// removes the failure mode where the agent's first turn depends on a package
/// mirror being up. It is also why [`Policy::provision`] defaults to `false`:
/// there is nothing left to provision.
pub const DEFAULT_IMAGE: &str = "ghcr.io/tobi/wrap:desktop";

/// Guest Unix account for wrap images. Agent identity home stays
/// `/workspace/agents/<id>/`; this is the OS user Chrome and git see.
pub const GUEST_USER: &str = "user";
pub const GUEST_HOME: &str = "/home/user";
pub const DESKTOP_DISPLAY: &str = ":1";
const DESKTOP_VNC_GUEST_PORT: u16 = 5900;
const DESKTOP_NOVNC_GUEST_PORT: u16 = 6080;

/// Name of the house tool that asks the user to allow a host. Referenced
/// from the gateway's 403 body so the model knows what to call.
pub const ASK_HOST_TOOL: &str = "AskForHostPermission";

/// The 403 body the gateway returns for a host outside the allow list.
/// `{host}` is substituted by microsandbox.
const HTTP_DENY_MESSAGE: &str = "\
This host is not allowed by the sandbox network policy config.\n\
\n\
Note to agent: `{host}` is not in the allowed-host list. Call the AskForHostPermission tool \
with this host and the reason you need it; the user decides. Do not retry the request until \
they have answered, and do not try to reach the host another way.\n";

/// The image ships a full Rust, Go, Node, and Python toolchain, so the
/// writable rootfs layer has to be big enough for a real build tree.
pub const DEFAULT_ROOT_DISK_MIB: u32 = 16 * 1024;

/// Packages for a bare image, if an agent points `sandbox.lua` at one.
pub const APT_PACKAGES: &[&str] = &[
    "ca-certificates",
    "curl",
    "git",
    "gh",
    "build-essential",
    "jq",
    "unzip",
    "ripgrep",
    "fd-find",
    "file",
    "less",
];
/// Node comes from mise. ast-grep stays on npm because mise's aqua backend
/// queries GitHub's unauthenticated, rate-limited releases API even for pinned
/// versions, and npm needs no implicit GitHub credential.
pub const MISE_TOOLS: &[&str] = &["node@lts"];
pub const NPM_TOOLS: &[&str] = &["@ast-grep/cli"];

const PROVISION_MARKER: &str = "/var/lib/reve/provisioned";

/// Teach git to read the token straight from the environment.
///
/// `--global` writes the user's `~/.gitconfig` so this works as unprivileged
/// `user`. A store-free helper: the token is already in the guest environment
/// as a microsandbox-resolved secret, so this leaves nothing extra on disk.
const GIT_CREDENTIAL_SETUP: &str = "if command -v git >/dev/null; then \
git config --global credential.https://github.com.helper \
'!f() { test \"$1\" = get && printf \"username=x-access-token\\npassword=%s\\n\" \
\"$GITHUB_TOKEN\"; }; f'; fi";

/// A host environment reference whose value is resolved only while the VM runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Secret {
    /// Environment variable exposed in the guest. Its value is a placeholder.
    pub env: String,
    /// How the host obtains the value: a host env var name, `$(command)`,
    /// `http(s)://…` (GET on the host), or `file:` relative to the house
    /// root (paste store under `.reve/secrets/`). Never a literal secret.
    /// Command/HTTP/file output is copied into a process env var so
    /// microsandbox still only sees [`SecretSource::Env`].
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Per-host scope. Keys are destination hostnames. Each host with
    /// `allow: true` also joins the sandbox network allow list.
    #[serde(default)]
    pub hosts: BTreeMap<String, SecretHost>,
}

/// Destination-host policy for one secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretHost {
    /// When true (the default), this host is on the sandbox network allow list.
    #[serde(default = "default_secret_host_allow")]
    pub allow: bool,
    /// Headers to send on requests to this host. Values may reference the
    /// secret as `$ENV` (e.g. `Authorization: "Bearer $TOOL_GATEWAY"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

fn default_secret_host_allow() -> bool {
    true
}

impl Default for SecretHost {
    fn default() -> Self {
        Self {
            allow: true,
            headers: BTreeMap::new(),
        }
    }
}

impl Secret {
    /// Hostnames this secret is scoped to, in sorted order.
    pub fn hostnames(&self) -> Vec<String> {
        self.hosts.keys().cloned().collect()
    }

    /// Hostnames that also join the sandbox network allow list.
    pub fn allowed_hostnames(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|(_, cfg)| cfg.allow)
            .map(|(host, _)| host.clone())
            .collect()
    }

    /// Build a host map with `allow: true` and no extra headers.
    pub fn scoped_hosts(
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> BTreeMap<String, SecretHost> {
        names
            .into_iter()
            .map(|name| (name.into(), SecretHost::default()))
            .collect()
    }
}

/// What `sandbox.lua` produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub image: String,
    /// Size of the writable rootfs layer, in MiB.
    #[serde(default = "default_root_disk")]
    pub root_disk: u32,
    pub cpus: u8,
    pub memory: u32,
    pub workdir: String,
    pub mount_workspace: bool,
    pub provision: bool,
    pub packages: Vec<String>,
    pub mise: Vec<String>,
    pub npm: Vec<String>,
    /// Public internet. The default. `false` plus `allow_hosts` is the lock-down.
    #[serde(default = "default_open")]
    pub open: bool,
    pub allow_hosts: Vec<String>,
    pub secrets: Vec<Secret>,
    pub bootstrap: Vec<String>,
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            image: DEFAULT_IMAGE.into(),
            root_disk: DEFAULT_ROOT_DISK_MIB,
            cpus: 2,
            memory: 8192,
            workdir: "/workspace".into(),
            mount_workspace: true,
            // The default image is already provisioned. An agent that points
            // at a bare image turns this back on in `sandbox.lua`.
            provision: false,
            packages: APT_PACKAGES.iter().map(|s| s.to_string()).collect(),
            mise: MISE_TOOLS.iter().map(|s| s.to_string()).collect(),
            npm: NPM_TOOLS.iter().map(|s| s.to_string()).collect(),
            open: true,
            allow_hosts: Vec::new(),
            secrets: Vec::new(),
            bootstrap: Vec::new(),
            env: BTreeMap::from([
                ("DEBIAN_FRONTEND".into(), "noninteractive".into()),
                ("MISE_YES".into(), "1".into()),
            ]),
            name: None,
        }
    }
}

fn default_root_disk() -> u32 {
    DEFAULT_ROOT_DISK_MIB
}

fn default_open() -> bool {
    true
}

impl Policy {
    /// Hosts named in `allow`, plus secret hosts with `allow: true`.
    /// When [`Self::open`] is true the guest can already reach the public
    /// internet; this list is extra (or the whole allowlist when `open` is
    /// false).
    pub fn egress_hosts(&self) -> Vec<String> {
        let mut hosts = self.allow_hosts.clone();
        for secret in &self.secrets {
            hosts.extend(secret.allowed_hostnames());
        }
        hosts.sort();
        hosts.dedup();
        hosts
    }

    pub fn egress_summary(&self) -> String {
        if self.open {
            "internet".into()
        } else {
            let hosts = self.egress_hosts();
            if hosts.is_empty() {
                "none".into()
            } else {
                hosts.join(", ")
            }
        }
    }

    /// Body the gateway returns to an HTTP(S) client inside the VM when a
    /// host is not on the allow list. Written for the model: it names the
    /// tool that asks the user for access. `{host}` is filled in by
    /// microsandbox.
    pub fn http_deny_message(&self) -> String {
        HTTP_DENY_MESSAGE.to_string()
    }

    pub fn internet_prompt(&self) -> String {
        if self.open {
            "You have internet access.".into()
        } else {
            let hosts = self.egress_hosts();
            if hosts.is_empty() {
                "You have no internet access.".into()
            } else {
                format!("You have internet access to {}.", hosts.join(", "))
            }
        }
    }

    /// A stable VM name per workspace, so a second launch restarts the
    /// provisioned VM instead of installing the toolchain again.
    pub fn sandbox_name(&self, host_workspace: &Path) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        let root = host_workspace.parent().unwrap_or(host_workspace);
        let label: String = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "agent".into())
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let digest = Sha256::digest(root.to_string_lossy().as_bytes());
        format!("reve-{label}-{}", &hex(&digest)[..10])
    }

    /// Identifies the *disk and VM shape*. Runtime environment and secret
    /// sources are deliberately excluded: they are refreshed live and must
    /// never force a rebuild or place credential material in this file.
    pub fn fingerprint(&self, host_workspace: &Path) -> String {
        let mut shape = self.clone();
        shape.secrets.clear();
        shape.env.clear();
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(&shape).unwrap_or_default());
        hasher.update(host_workspace.to_string_lossy().as_bytes());
        hex(&hasher.finalize())
    }

    /// One shell command that installs the default toolchain, guarded by a
    /// marker so it runs once per VM and keeps the boot path short.
    pub fn provision_script(&self) -> String {
        let packages = self.packages.join(" ");
        let mise = self.mise.join(" ");
        let npm = self.npm.join(" ");
        let mise_step = if mise.is_empty() {
            String::new()
        } else {
            format!("retry mise use -g {mise} > /dev/null")
        };
        let npm_step = if npm.is_empty() {
            String::new()
        } else {
            format!("retry npm install -g {npm} > /dev/null")
        };
        format!(
            r#"set -e
[ -f {PROVISION_MARKER} ] && exit 0
retry() {{
  attempts=0
  until "$@"; do
    attempts=$((attempts + 1))
    [ "$attempts" -ge 5 ] && return 1
    sleep "$attempts"
  done
}}
mkdir -p /var/lib/reve
if command -v apt-get > /dev/null; then
  apt-get -o Acquire::Retries=5 update -qq
  apt-get -o Acquire::Retries=5 install -y --no-install-recommends {packages} > /dev/null
  [ -x /usr/bin/fdfind ] && ln -sf /usr/bin/fdfind /usr/local/bin/fd || true
fi
if ! command -v mise > /dev/null; then
  retry curl -fsSL --retry 5 --retry-all-errors --retry-delay 1 -o /tmp/mise-install.sh https://mise.run
  retry env MISE_INSTALL_PATH=/usr/local/bin/mise sh /tmp/mise-install.sh
  rm -f /tmp/mise-install.sh
fi
printf '%s\n' 'export PATH="/usr/local/bin:$HOME/.local/share/mise/shims:$PATH"' > /etc/profile.d/10-mise.sh
chmod +x /etc/profile.d/10-mise.sh
. /etc/profile.d/10-mise.sh
{mise_step}
{npm_step}
touch {PROVISION_MARKER}
"#
        )
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The result of running a command in the VM.
///
/// A non-zero exit is **data**, not an error: the model reads the code and
/// stderr and decides what to do next.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub success: bool,
    #[serde(default)]
    pub cancelled: bool,
}

/// Options for one command.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout: Option<std::time::Duration>,
}

/// Host-side VNC/noVNC listeners for a desktop guest. CDP stays on guest
/// localhost:9222; it is not published to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desktop {
    pub novnc_port: u16,
    pub vnc_port: u16,
}

impl Desktop {
    pub fn novnc_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/vnc.html?autoconnect=1&resize=scale&reconnect=1",
            self.novnc_port
        )
    }

    pub fn vnc_addr(&self) -> String {
        format!("127.0.0.1:{}", self.vnc_port)
    }
}

/// A microVM that starts on its first effect and stops after a short idle
/// window. `start` still boots once up front, so Reve fails closed when the VM
/// runtime or policy is unavailable.
pub struct Sandbox {
    policy: Policy,
    host_workspace: PathBuf,
    /// House root (parent of `.reve/`). `file:` secret sources resolve here.
    secret_root: PathBuf,
    /// Live secret list. Starts as `policy.secrets`; AskUserForSecret upserts
    /// here so a save takes effect without rebuilding the VM fingerprint.
    secrets: parking_lot::Mutex<Vec<Secret>>,
    name: String,
    desktop: Option<Desktop>,
    vm: Arc<Mutex<VmState>>,
}

struct VmState {
    vm: Option<MsbSandbox>,
    /// Live effects: execs and file operations that hold a cloned handle.
    active: usize,
    /// [`Sandbox::hold`]s. A hold keeps the guest from idle-stopping but is
    /// not an effect: it must not stop a secret-rotation restart, or a house
    /// (which holds for its whole lifetime) could never pick up a saved
    /// secret. `docs/tla/VmLifecycle.tla` `InvIdleAcquireIsFresh`.
    holds: usize,
    generation: u64,
    /// Digest of the secret values the *running guest* was started with.
    /// Cleared on every stop; compared against the host at every acquire.
    secret_digests: BTreeMap<String, String>,
}

impl VmState {
    fn begin(&mut self) {
        self.active += 1;
        self.generation = self.generation.wrapping_add(1);
    }

    fn finish(&mut self) -> Option<u64> {
        self.active = self.active.saturating_sub(1);
        self.generation = self.generation.wrapping_add(1);
        (self.active == 0 && self.holds == 0).then_some(self.generation)
    }

    fn hold(&mut self) {
        self.holds += 1;
        self.generation = self.generation.wrapping_add(1);
    }

    fn release_hold(&mut self) -> Option<u64> {
        self.holds = self.holds.saturating_sub(1);
        self.generation = self.generation.wrapping_add(1);
        (self.active == 0 && self.holds == 0).then_some(self.generation)
    }

    /// No effect is live; the guest may be stopped and restarted to pick up
    /// changed secret sources. Holds do not count.
    fn effect_idle(&self) -> bool {
        self.active == 0
    }

    fn may_stop(&self, generation: u64) -> bool {
        self.active == 0 && self.holds == 0 && self.generation == generation
    }
}

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

impl Sandbox {
    /// Boot the VM: restart the persisted one when the policy is unchanged,
    /// otherwise build a fresh one and provision it.
    pub async fn start(
        policy: Policy,
        host_workspace: impl AsRef<Path>,
        state_dir: impl AsRef<Path>,
        progress: &dyn Progress,
    ) -> Result<Self> {
        let host_workspace = host_workspace.as_ref().to_path_buf();
        let name = policy.sandbox_name(&host_workspace);
        tokio::fs::create_dir_all(&host_workspace)
            .await
            .map_err(|e| SandboxError::Unavailable(format!("cannot create workspace: {e}")))?;
        let secret_root = secret_root_from(state_dir.as_ref());
        for secret in &policy.secrets {
            if let Some(warning) = missing_secret_warning(secret, &secret_root) {
                progress.warning(&warning);
            }
        }

        if !microsandbox::setup::is_installed() {
            progress.stage("installing the microsandbox runtime");
            microsandbox::setup::install()
                .await
                .map_err(|e| SandboxError::Unavailable(format!("cannot install runtime: {e}")))?;
        }
        // Never adopt a VM another Reve owns: replacing it would break
        // isolation for both processes.
        if let Ok(handle) = MsbSandbox::get(&name).await {
            use microsandbox::sandbox::SandboxStatus;
            if matches!(
                handle.status_snapshot(),
                SandboxStatus::Running | SandboxStatus::Draining
            ) {
                return Err(SandboxError::Unavailable(format!(
                    "microVM {name} is already running in another Reve process"
                )));
            }
        }

        let fingerprint_path = state_dir.as_ref().join("sandbox-fingerprint");
        let fingerprint = policy.fingerprint(&host_workspace);
        let reusable = tokio::fs::read_to_string(&fingerprint_path)
            .await
            .map(|text| text.trim() == fingerprint)
            .unwrap_or(false);
        if reusable {
            // Refresh source references while stopped. Microsandbox resolves
            // their values only when the VM starts; no credential is persisted.
            if let Ok(handle) = MsbSandbox::get(&name).await {
                let config = handle.config().map_err(secret_config_error)?;
                let existing = persisted_secret_names(&config);
                remove_secret_definitions(handle.modify(), &existing, true).await?;
                install_secret_definitions(handle.modify(), &policy.secrets, true, &secret_root)
                    .await?;
            }
            progress.stage(&format!("restarting microVM {name}"));
            if let Ok(vm) = MsbSandbox::start(&name).await {
                let desktop = desktop_from_config(vm.config());
                let secret_digests = runtime_secret_digests(&policy.secrets, &secret_root);
                let sandbox = Self {
                    secrets: parking_lot::Mutex::new(policy.secrets.clone()),
                    secret_root,
                    policy,
                    host_workspace,
                    name,
                    desktop,
                    vm: Arc::new(Mutex::new(VmState {
                        vm: Some(vm),
                        active: 0,
                        holds: 0,
                        generation: 0,
                        secret_digests,
                    })),
                };
                sandbox.prepare_guest().await?;
                progress.finish("sandbox ready");
                return Ok(sandbox);
            }
        }

        // `build` replaces the persisted definition and disk under the same
        // name. If provisioning fails or the process dies before the new
        // fingerprint is written, a stale file naming an *older* policy would
        // survive; reverting config.yml to that policy would then reuse a
        // disk built for a different one (`docs/tla/VmLifecycle.tla`
        // `InvFingerprintHonest`). Forget the old promise before breaking it.
        forget_fingerprint(&fingerprint_path).await?;
        progress.stage(&format!("building microVM {name} from {}", policy.image));
        let (vm, desktop) = build(&policy, &name, &host_workspace, &secret_root).await?;
        let secret_digests = runtime_secret_digests(&policy.secrets, &secret_root);
        let sandbox = Self {
            secrets: parking_lot::Mutex::new(policy.secrets.clone()),
            secret_root,
            policy,
            host_workspace,
            name,
            desktop,
            vm: Arc::new(Mutex::new(VmState {
                vm: Some(vm),
                active: 0,
                holds: 0,
                generation: 0,
                secret_digests,
            })),
        };

        let mut ok = true;
        if sandbox.policy.provision {
            let tools = sandbox
                .policy
                .mise
                .iter()
                .chain(sandbox.policy.npm.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            progress.stage(&format!(
                "provisioning APT packages{}",
                if tools.is_empty() {
                    String::new()
                } else {
                    format!(" and {tools}")
                }
            ));
            ok = sandbox.provision().await;
        }
        for (index, command) in sandbox.policy.bootstrap.iter().enumerate() {
            progress.stage(&format!(
                "running bootstrap {}/{}: {}",
                index + 1,
                sandbox.policy.bootstrap.len(),
                command.lines().next().unwrap_or("")
            ));
            let result = sandbox.exec(command, ExecOptions::default(), None).await?;
            ok &= result.success;
        }

        if ok {
            if let Some(parent) = fingerprint_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&fingerprint_path, format!("{fingerprint}\n")).await;
        }
        sandbox.prepare_guest().await?;
        progress.finish(if ok {
            "sandbox ready"
        } else {
            "sandbox ready with provisioning errors"
        });
        Ok(sandbox)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn workdir(&self) -> &str {
        &self.policy.workdir
    }

    pub fn host_workspace(&self) -> &Path {
        &self.host_workspace
    }

    /// Published desktop ports, if this guest is a wrap desktop image.
    pub fn desktop(&self) -> Option<Desktop> {
        self.desktop
    }

    /// Keep the guest from idle-stopping for the lifetime of the house. A
    /// hold is not an effect: a secret change still restarts the guest at the
    /// next effect-idle acquire.
    pub async fn hold(&self) -> Result<()> {
        let _ = self.acquire().await?;
        let mut state = self.vm.lock().await;
        state.hold();
        // acquire() counted one effect; the hold replaces it.
        let _ = state.finish();
        Ok(())
    }

    /// Pair of [`hold`]: allow idle-stop again.
    pub async fn release_hold(&self) {
        let generation = {
            let mut state = self.vm.lock().await;
            state.release_hold()
        };
        if let Some(generation) = generation {
            self.schedule_idle_stop(generation);
        }
    }

    /// If a namesake is still Running after a dead house, stop it so
    /// [`Sandbox::start`] can boot. Only the lock holder should call this.
    pub async fn reclaim_namesake(name: &str) -> Result<()> {
        let Ok(handle) = MsbSandbox::get(name).await else {
            return Ok(());
        };
        use microsandbox::sandbox::SandboxStatus;
        if matches!(
            handle.status_snapshot(),
            SandboxStatus::Running | SandboxStatus::Draining
        ) {
            handle
                .stop()
                .await
                .map_err(|e| SandboxError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    pub fn sandbox_name_for(policy: &Policy, host_workspace: impl AsRef<Path>) -> String {
        policy.sandbox_name(host_workspace.as_ref())
    }

    /// Run a command through `sh -lc`, so the guest's login PATH (and therefore
    /// mise's shims) is in effect.
    ///
    /// `cancel` is what makes `/abort` real: it kills the guest command through
    /// the agent's own control channel instead of abandoning the caller while
    /// the VM keeps working.
    pub async fn exec(
        &self,
        command: &str,
        options: ExecOptions,
        cancel: Option<tokio_util_lite::CancelRx>,
    ) -> Result<Output> {
        let vm = self.acquire().await?;
        let result = async {
            let cwd = options
                .cwd
                .clone()
                .unwrap_or_else(|| self.policy.workdir.clone());
            let script = command.to_string();
            let mut env = guest_unix_env(&self.policy.image);
            env.extend(self.policy.env.clone());
            env.extend(options.env.clone());
            let timeout = options.timeout;

            let mut handle = vm
                .exec_stream_with("sh", move |mut e| {
                    e = e.args(["-lc", script.as_str()]).cwd(cwd).stdin_null();
                    for (key, value) in &env {
                        e = e.env(key.as_str(), value.as_str());
                    }
                    if let Some(t) = timeout {
                        e = e.timeout(t);
                    }
                    e
                })
                .await
                .map_err(|e| SandboxError::Failed(e.to_string()))?;

            let control = handle.control();
            match cancel {
                None => {
                    let output = handle
                        .collect()
                        .await
                        .map_err(|e| SandboxError::Failed(e.to_string()))?;
                    Ok(encode(&output, false))
                }
                Some(mut rx) => {
                    tokio::select! {
                        collected = handle.collect() => {
                            let output =
                                collected.map_err(|e| SandboxError::Failed(e.to_string()))?;
                            Ok(encode(&output, false))
                        }
                        _ = rx.cancelled() => {
                            let _ = control.kill().await;
                            Ok(Output {
                                stdout: String::new(),
                                stderr: String::new(),
                                exit_code: 130,
                                success: false,
                                cancelled: true,
                            })
                        }
                    }
                }
            }
        }
        .await;
        self.release().await;
        result
    }

    pub async fn read_file(&self, path: &str) -> Result<String> {
        let vm = self.acquire().await?;
        let result = vm
            .fs()
            .read_to_string(&self.absolute(path))
            .await
            .map_err(|e| SandboxError::Failed(e.to_string()));
        self.release().await;
        result
    }

    pub async fn write_file(&self, path: &str, content: &str) -> Result<()> {
        let vm = self.acquire().await?;
        let result = vm
            .fs()
            .write(&self.absolute(path), content.as_bytes())
            .await
            .map_err(|e| SandboxError::Failed(e.to_string()));
        self.release().await;
        result
    }

    /// Stop the VM but keep its root disk and source-only secret definitions,
    /// so the next effect restarts it with values resolved from the current
    /// host environment. Idempotent.
    pub async fn stop(&self) -> Result<()> {
        // Keep the lifecycle lock until microsandbox confirms the stop. If the
        // handle were removed first, a simultaneous effect could try to start
        // the persisted definition while its previous process was still
        // draining and receive "sandbox still running".
        let mut state = self.vm.lock().await;
        state.active = 0;
        state.holds = 0;
        state.generation = state.generation.wrapping_add(1);
        state.secret_digests.clear();
        match state.vm.take() {
            Some(vm) => vm
                .stop()
                .await
                .map_err(|e| SandboxError::Failed(e.to_string())),
            None => Ok(()),
        }
    }

    pub fn describe(&self) -> String {
        let mut extras = Vec::new();
        if self.policy.provision {
            extras.push("provisioned".to_string());
        }
        if !self.policy.mise.is_empty() {
            extras.push(format!("mise {}", self.policy.mise.join(",")));
        }
        extras.push(format!("net {}", self.policy.egress_summary()));
        extras.push(format!("idle {}s", IDLE_TIMEOUT.as_secs()));
        if let Some(desktop) = self.desktop {
            extras.push(format!("novnc {}", desktop.novnc_url()));
        }
        let secrets = self.secrets();
        if !secrets.is_empty() {
            let names: Vec<&str> = secrets.iter().map(|s| s.env.as_str()).collect();
            extras.push(format!("secrets {}", names.join(",")));
        }
        let mount = if self.policy.mount_workspace {
            format!(
                "bind {} → {} (rw)",
                self.host_workspace.display(),
                self.policy.workdir
            )
        } else {
            "no workspace mount".to_string()
        };
        format!(
            "microsandbox {} ({} cpu, {}MB, {}) {mount}",
            self.policy.image,
            self.policy.cpus,
            self.policy.memory,
            extras.join(", ")
        )
    }

    pub fn secrets(&self) -> Vec<Secret> {
        self.secrets.lock().clone()
    }

    /// Merge `secret` by `env` and reinstall host-side sources on the live VM
    /// definition. The guest still only sees a placeholder.
    pub async fn upsert_secret(&self, secret: Secret) -> Result<()> {
        {
            let mut secrets = self.secrets.lock();
            if let Some(existing) = secrets.iter_mut().find(|s| s.env == secret.env) {
                *existing = secret.clone();
            } else {
                secrets.push(secret.clone());
            }
        }
        let secrets = self.secrets();
        let state = self.vm.lock().await;
        if let Some(vm) = state.vm.as_ref() {
            // `next_start`: the definition changes now, the running guest does
            // not. `secret_digests` keeps describing the guest, so the next
            // effect-idle acquire() sees the difference and restarts. Claiming
            // the new digest here would make a saved secret invisible for as
            // long as the house holds the VM (`docs/tla/VmLifecycle.tla`
            // `InvDigestsDescribeGuest`).
            let config = vm.config();
            let existing = persisted_secret_names(config);
            remove_secret_definitions(vm.modify(), &existing, true).await?;
            install_secret_definitions(vm.modify(), &secrets, true, &self.secret_root).await?;
        }
        Ok(())
    }

    fn absolute(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{path}", self.policy.workdir.trim_end_matches('/'))
        }
    }

    /// Start on demand, then account for one effect. Holding the mutex across
    /// `start` serializes simultaneous first effects inside this process.
    async fn acquire(&self) -> Result<MsbSandbox> {
        let mut state = self.vm.lock().await;
        let secrets = self.secrets();
        let desired = runtime_secret_digests(&secrets, &self.secret_root);
        if state.vm.is_none() {
            if let Ok(handle) = MsbSandbox::get(&self.name).await {
                let config = handle.config().map_err(secret_config_error)?;
                let existing = persisted_secret_names(&config);
                remove_secret_definitions(handle.modify(), &existing, true).await?;
                install_secret_definitions(handle.modify(), &secrets, true, &self.secret_root)
                    .await?;
            }
            state.vm = Some(MsbSandbox::start(&self.name).await.map_err(|error| {
                SandboxError::Unavailable(format!("cannot start microVM {}: {error}", self.name))
            })?);
            state.secret_digests = desired;
        } else if state.effect_idle() && state.secret_digests != desired {
            let vm = state.vm.take().expect("checked above");
            vm.stop()
                .await
                .map_err(|e| SandboxError::Failed(e.to_string()))?;
            let config = vm.config();
            let existing = persisted_secret_names(config);
            remove_secret_definitions(vm.modify(), &existing, true).await?;
            install_secret_definitions(vm.modify(), &secrets, true, &self.secret_root).await?;
            state.vm = Some(MsbSandbox::start(&self.name).await.map_err(|error| {
                SandboxError::Unavailable(format!(
                    "cannot restart microVM {} after secret rotation: {error}",
                    self.name
                ))
            })?);
            state.secret_digests = desired;
        }
        state.begin();
        Ok(state.vm.as_ref().expect("set above").clone())
    }

    async fn release(&self) {
        let generation = {
            let mut state = self.vm.lock().await;
            state.finish()
        };
        if let Some(generation) = generation {
            self.schedule_idle_stop(generation);
        }
    }

    fn schedule_idle_stop(&self, generation: u64) {
        let state = Arc::clone(&self.vm);
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_TIMEOUT).await;
            // Starting and stopping are one serialized lifecycle transition.
            // New effects wait here, then restart the fully stopped VM; active
            // effects still share the existing cloned handle concurrently.
            let mut state = state.lock().await;
            if !state.may_stop(generation) {
                return;
            }
            state.secret_digests.clear();
            if let Some(vm) = state.vm.take() {
                let _ = vm.stop().await;
            }
        });
    }

    async fn provision(&self) -> bool {
        let script = self.policy.provision_script();
        let options = ExecOptions {
            cwd: Some("/".into()),
            timeout: Some(std::time::Duration::from_secs(900)),
            ..Default::default()
        };
        match self.exec(&script, options, None).await {
            Ok(output) if output.success => true,
            Ok(output) => {
                eprintln!(
                    "\x1b[33m sandbox provisioning failed (exit {}): {}\x1b[0m",
                    output.exit_code,
                    output
                        .stderr
                        .lines()
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                false
            }
            Err(e) => {
                eprintln!("\x1b[33m sandbox provisioning failed: {e}\x1b[0m");
                false
            }
        }
    }

    async fn prepare_guest(&self) -> Result<()> {
        self.align_guest_identity().await?;
        self.configure_git_credentials().await;
        self.ensure_desktop().await;
        Ok(())
    }

    async fn configure_git_credentials(&self) {
        let options = ExecOptions {
            cwd: Some("/".into()),
            ..Default::default()
        };
        let _ = self.exec(GIT_CREDENTIAL_SETUP, options, None).await;
    }

    /// Realign guest `user` to the host owner of `/workspace` so the virtiofs
    /// bind (stat virtualization off) is writable without chowning host files.
    async fn align_guest_identity(&self) -> Result<()> {
        if !is_wrap_image(&self.policy.image) {
            return Ok(());
        }
        let Some((uid, gid)) = host_identity(&self.host_workspace) else {
            return Ok(());
        };
        if uid == 0 {
            return Ok(());
        }
        let script = format!(
            "set -eu\n\
             uid={uid}; gid={gid}\n\
             cur_gid=$(getent group {user} | cut -d: -f3)\n\
             if [ \"$cur_gid\" != \"$gid\" ]; then\n\
               if getent group \"$gid\" >/dev/null; then groupmod -o -g \"$gid\" {user}; else groupmod -g \"$gid\" {user}; fi\n\
             fi\n\
             cur_uid=$(id -u {user})\n\
             if [ \"$cur_uid\" != \"$uid\" ]; then usermod -o -u \"$uid\" -g \"$gid\" {user}; fi\n\
             if [ \"$cur_uid\" != \"$uid\" ] || [ \"$cur_gid\" != \"$gid\" ]; then\n\
               chown -R \"$uid:$gid\" {home} /opt/mise\n\
             fi\n",
            user = GUEST_USER,
            home = GUEST_HOME,
        );
        let output = self.exec_as("root", &script).await?;
        if !output.success {
            return Err(SandboxError::Failed(format!(
                "align guest user identity exited {}: {}",
                output.exit_code,
                output.stderr.trim()
            )));
        }
        Ok(())
    }

    async fn ensure_desktop(&self) {
        if self.desktop.is_none() {
            return;
        }
        let options = ExecOptions {
            cwd: Some("/".into()),
            timeout: Some(Duration::from_secs(45)),
            ..Default::default()
        };
        let _ = self
            .exec(
                "command -v wrap-desktop >/dev/null && wrap-desktop start --quiet || true",
                options,
                None,
            )
            .await;
    }

    async fn exec_as(&self, user: &str, command: &str) -> Result<Output> {
        let vm = self.acquire().await?;
        let owned_user = user.to_string();
        let owned_cmd = command.to_string();
        let result = async {
            let mut handle = vm
                .exec_stream_with("sh", move |e| {
                    e.args(["-c", owned_cmd.as_str()])
                        .user(owned_user.as_str())
                        .cwd("/")
                        .stdin_null()
                })
                .await
                .map_err(|e| SandboxError::Failed(e.to_string()))?;
            let output = handle
                .collect()
                .await
                .map_err(|e| SandboxError::Failed(e.to_string()))?;
            Ok(encode(&output, false))
        }
        .await;
        self.release().await;
        result
    }
}

fn encode(output: &microsandbox::ExecOutput, cancelled: bool) -> Output {
    let status = output.status();
    Output {
        stdout: String::from_utf8_lossy(output.stdout_bytes()).into_owned(),
        stderr: String::from_utf8_lossy(output.stderr_bytes()).into_owned(),
        exit_code: status.code,
        success: status.success,
        cancelled,
    }
}

fn secret_root_from(state_dir: &Path) -> PathBuf {
    state_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.to_path_buf())
}

fn runtime_secret_digests(secrets: &[Secret], root: &Path) -> BTreeMap<String, String> {
    secrets
        .iter()
        .filter_map(|secret| {
            let value = secret_value(secret, root)?;
            Some((secret.env.clone(), hex(&Sha256::digest(value.as_bytes()))))
        })
        .collect()
}

/// `$(command)` as the entire `source`, no nested substitution.
pub(crate) fn command_secret_source(source: &str) -> Option<&str> {
    let trimmed = source.trim();
    let inner = trimmed.strip_prefix("$(")?.strip_suffix(')')?;
    if inner.is_empty() || inner.contains("$(") {
        return None;
    }
    Some(inner.trim())
}

fn host_var_for_command(secret_env: &str) -> String {
    let cleaned: String = secret_env
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    format!("REVEBOT_SECRET_{cleaned}")
}

/// `~` and `~/…` only. `$(command)` is argv, not a shell, so tilde would
/// otherwise stay literal (`cat '~/.cache/token'`).
fn expand_tilde(arg: &str) -> String {
    let home = || std::env::var("HOME").ok().filter(|s| !s.is_empty());
    if arg == "~" {
        return home().unwrap_or_else(|| arg.to_string());
    }
    if let Some(rest) = arg.strip_prefix("~/")
        && let Some(home) = home()
    {
        let home = home.trim_end_matches('/');
        return format!("{home}/{rest}");
    }
    arg.to_string()
}

fn run_secret_command(script: &str) -> Result<String, String> {
    let words = shell_words::split(script).map_err(|e| e.to_string())?;
    let words: Vec<String> = words.iter().map(|w| expand_tilde(w)).collect();
    let Some(program) = words.first() else {
        return Err("empty command".into());
    };
    let output = std::process::Command::new(program)
        .args(&words[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let err = err.trim();
        let code = output.status.code().unwrap_or(-1);
        if err.is_empty() {
            return Err(format!("exit {code}"));
        }
        return Err(format!("exit {code}: {err}"));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Err("produced no output".into());
    }
    if value.len() > 16 * 1024 {
        return Err("output too large".into());
    }
    Ok(value)
}

fn file_secret_path(source: &str, root: &Path) -> Option<PathBuf> {
    let rest = source.strip_prefix("file:")?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let expanded = expand_tilde(rest);
    let path = Path::new(&expanded);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

fn http_secret_url(source: &str) -> Option<&str> {
    let url = source.trim();
    let url = url
        .strip_prefix("http:")
        .filter(|rest| rest.starts_with("http://") || rest.starts_with("https://"))
        .unwrap_or(url);
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    if url
        .chars()
        .any(|c| c.is_ascii_whitespace() || matches!(c, ';' | '|' | '&' | '`'))
    {
        return None;
    }
    Some(url)
}

fn fetch_http_secret(url: &str) -> Result<String, String> {
    let output = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "15", "--", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !output.status.success() {
        return Err(format!("exit {}", output.status.code().unwrap_or(-1)));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Err("produced no output".into());
    }
    if value.len() > 16 * 1024 {
        return Err("output too large".into());
    }
    Ok(value)
}

fn read_file_secret(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn secret_value(secret: &Secret, root: &Path) -> Option<String> {
    if let Some(script) = command_secret_source(&secret.source) {
        run_secret_command(script).ok().filter(|s| !s.is_empty())
    } else if let Some(path) = file_secret_path(&secret.source, root) {
        read_file_secret(&path)
    } else if let Some(url) = http_secret_url(&secret.source) {
        fetch_http_secret(url).ok().filter(|s| !s.is_empty())
    } else {
        std::env::var(&secret.source).ok()
    }
}

/// Resolve a secret to a host env var microsandbox can read. Command, file, and
/// HTTP sources are copied into `REVEBOT_SECRET_<env>` for this process.
fn bind_secret(secret: &Secret, root: &Path) -> Option<String> {
    let needs_copy = command_secret_source(&secret.source).is_some()
        || file_secret_path(&secret.source, root).is_some()
        || http_secret_url(&secret.source).is_some();
    if needs_copy {
        let value = secret_value(secret, root)?;
        let var = host_var_for_command(&secret.env);
        // SAFETY: we only write Reve-owned `REVEBOT_SECRET_*` keys. Secret
        // install is serialized by the VM mutex; other threads may read env
        // concurrently, which is the documented hazard of `set_var`.
        unsafe { std::env::set_var(&var, &value) };
        Some(var)
    } else {
        std::env::var_os(&secret.source)?;
        Some(secret.source.clone())
    }
}
fn secret_config_error(error: microsandbox::MicrosandboxError) -> SandboxError {
    SandboxError::Failed(format!("cannot inspect runtime secrets: {error}"))
}

fn persisted_secret_names(config: &microsandbox::SandboxConfig) -> Vec<String> {
    config
        .spec
        .network
        .secrets
        .iter()
        .flat_map(|config| &config.secrets)
        .map(|secret| secret.env_var.clone())
        .collect()
}

async fn remove_secret_definitions(
    mut modification: SandboxModificationBuilder,
    names: &[String],
    next_start: bool,
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    for name in names {
        modification = modification.remove_secret(name.as_str());
    }
    if next_start {
        modification = modification.next_start();
    }
    modification
        .apply()
        .await
        .map(|_| ())
        .map_err(|e| SandboxError::Failed(format!("cannot remove runtime secrets: {e}")))
}

async fn install_secret_definitions(
    mut modification: SandboxModificationBuilder,
    secrets: &[Secret],
    next_start: bool,
    root: &Path,
) -> Result<()> {
    let available: Vec<(Secret, String)> = secrets
        .iter()
        .cloned()
        .filter_map(|secret| {
            let host_var = bind_secret(&secret, root)?;
            Some((secret, host_var))
        })
        .collect();
    if available.is_empty() {
        return Ok(());
    }
    for (secret, host_var) in available {
        modification = modification.secret(move |mut patch| {
            patch = patch
                .env(secret.env.as_str())
                .source(SecretSource::Env { var: host_var });
            if let Some(placeholder) = &secret.placeholder {
                patch = patch.placeholder(placeholder.as_str());
            }
            for host in secret.hosts.keys() {
                patch = patch.allow_host(host.as_str());
            }
            patch
        });
    }
    if next_start {
        modification = modification.next_start();
    }
    modification
        .apply()
        .await
        .map(|_| ())
        .map_err(|e| SandboxError::Failed(format!("cannot apply runtime secrets: {e}")))
}

fn missing_secret_warning(secret: &Secret, root: &Path) -> Option<String> {
    secret_value(secret, root)
        .is_none()
        .then(|| format_missing_secret_warning(secret))
}

fn format_missing_secret_warning(secret: &Secret) -> String {
    let hosts = secret.hostnames().join(", ");
    if let Some(script) = command_secret_source(&secret.source) {
        let detail = match run_secret_command(script) {
            Ok(value) if value.is_empty() => "produced no output".to_string(),
            Ok(_) => "unavailable".to_string(),
            Err(e) => e,
        };
        return format!(
            "{} failed ({detail}); authenticated access for {hosts} is disabled",
            secret.source
        );
    }
    let mut warning = format!(
        "{} is unset; authenticated access for {hosts} is disabled",
        secret.source
    );
    if secret.source == "GITHUB_TOKEN" {
        warning.push_str("\nexport GITHUB_TOKEN=\"$(gh auth token)\"");
    } else if secret.source == "OPENROUTER_API_KEY" {
        warning.push_str("\nexport OPENROUTER_API_KEY=...");
    }
    warning
}

fn is_wrap_image(image: &str) -> bool {
    image.contains("tobi/wrap") || image == DEFAULT_IMAGE
}

fn is_desktop_image(image: &str) -> bool {
    image.contains("wrap:desktop") || image == DEFAULT_IMAGE
}

fn guest_unix_env(image: &str) -> BTreeMap<String, String> {
    if !is_wrap_image(image) {
        return BTreeMap::new();
    }
    BTreeMap::from([
        ("HOME".into(), GUEST_HOME.into()),
        ("USER".into(), GUEST_USER.into()),
        ("DISPLAY".into(), DESKTOP_DISPLAY.into()),
        ("XDG_SESSION_TYPE".into(), "x11".into()),
    ])
}

fn host_identity(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.uid(), meta.gid()))
}

fn reserve_localhost_ports(count: usize) -> Result<Vec<u16>> {
    let mut held = Vec::with_capacity(count);
    for _ in 0..count {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| {
            SandboxError::Unavailable(format!("cannot reserve localhost port: {e}"))
        })?;
        let _ = listener.set_nonblocking(true);
        held.push(listener);
    }
    let mut ports = Vec::with_capacity(count);
    for listener in &held {
        ports.push(
            listener
                .local_addr()
                .map_err(|e| SandboxError::Unavailable(format!("cannot read reserved port: {e}")))?
                .port(),
        );
    }
    Ok(ports)
}

fn desktop_from_config(config: &microsandbox::SandboxConfig) -> Option<Desktop> {
    let mut novnc = None;
    let mut vnc = None;
    for port in &config.spec.network.ports {
        match port.guest_port {
            DESKTOP_NOVNC_GUEST_PORT => novnc = Some(port.host_port),
            DESKTOP_VNC_GUEST_PORT => vnc = Some(port.host_port),
            _ => {}
        }
    }
    Some(Desktop {
        novnc_port: novnc?,
        vnc_port: vnc?,
    })
}

/// Remove the fingerprint file so no policy is vouched for while the disk is
/// being replaced. A missing file is already the wanted state.
async fn forget_fingerprint(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SandboxError::Unavailable(format!(
            "cannot invalidate sandbox fingerprint {}: {e}",
            path.display()
        ))),
    }
}

/// Turn a [`Policy`] into a booted VM.
async fn build(
    policy: &Policy,
    name: &str,
    host_workspace: &Path,
    secret_root: &Path,
) -> Result<(MsbSandbox, Option<Desktop>)> {
    let wrap = is_wrap_image(&policy.image);
    let host_uid = host_identity(host_workspace)
        .map(|(uid, _)| uid)
        .unwrap_or(0);
    let mut builder = MsbSandbox::builder(name.to_string())
        .image(policy.image.clone())
        .root_disk(policy.root_disk.mib())
        .cpus(policy.cpus)
        .memory(policy.memory)
        .workdir(policy.workdir.clone())
        .replace();

    if wrap && host_uid != 0 {
        builder = builder.user(GUEST_USER);
    }

    // Ordinary environment values are exec-time parameters, not VM state.

    if policy.mount_workspace {
        let host = host_workspace.to_path_buf();
        builder = builder.volume(policy.workdir.clone(), move |m| {
            let mounted = m.bind(host);
            if wrap {
                mounted.stat_virtualization(StatVirtualization::Off)
            } else {
                mounted
            }
        });
    }

    let desktop_ports = if is_desktop_image(&policy.image) {
        let ports = reserve_localhost_ports(2)?;
        Some(Desktop {
            novnc_port: ports[0],
            vnc_port: ports[1],
        })
    } else {
        None
    };

    // Open (the default): public internet + gateway DNS.
    // Locked down: deny both directions, gateway DNS, then named hosts.
    let mut network = if policy.open {
        NetworkPolicy::from_profiles([NetworkProfile::Public])
    } else {
        let mut network = NetworkPolicy::none();
        network.rules.push(Rule::allow_dns());
        network
    };
    let hosts = policy.egress_hosts();
    if !hosts.is_empty() {
        network = network
            .allow_domains(hosts)
            .map_err(|e| SandboxError::Policy(format!("invalid allowed host: {e}")))?;
    }
    // Locked down: intercept TLS so a denied HTTPS request is answered with
    // the 403 note in-tunnel instead of a bare reset (agentd installs the
    // intercept CA into the guest trust store). The SDK already turns
    // interception on whenever a secret is configured; this makes the
    // allow-list mode consistent with or without secrets. Open policies
    // never deny by default, so they are left alone.
    let intercept_tls = !policy.open;
    let deny_message = policy.http_deny_message();
    builder = builder.network(move |mut n| {
        n = n
            .enabled(true)
            .policy(network)
            .http_deny_message(deny_message);
        if intercept_tls {
            n = n.tls(|t| t);
        }
        n
    });
    if let Some(desktop) = desktop_ports {
        // After network(): `.network()` replaces the local config, so ports
        // have to land on the builder afterwards.
        builder = builder
            .port(desktop.novnc_port, DESKTOP_NOVNC_GUEST_PORT)
            .port(desktop.vnc_port, DESKTOP_VNC_GUEST_PORT);
    }

    // Only host environment references enter the durable definition. Values
    // are resolved by microsandbox when the VM starts and remain host-side.
    for secret in policy.secrets.clone() {
        let Some(host_var) = bind_secret(&secret, secret_root) else {
            continue;
        };
        builder = builder.secret(move |mut entry| {
            entry = entry
                .env(secret.env.as_str())
                .source(SecretSource::Env { var: host_var });
            if let Some(placeholder) = &secret.placeholder {
                entry = entry.placeholder(placeholder.as_str());
            }
            for host in secret.hosts.keys() {
                entry = entry.allow_host(host.as_str());
            }
            entry
        });
    }
    let vm = builder
        .create()
        .await
        .map_err(|e| SandboxError::Unavailable(e.to_string()))?;
    Ok((vm, desktop_ports))
}

/// Startup happens before the TUI exists, so a long image pull needs somewhere
/// to say so.
pub trait Progress: Send + Sync {
    fn warning(&self, label: &str);
    fn stage(&self, label: &str);
    fn finish(&self, label: &str);
}

/// Progress that says nothing — for tests and non-interactive runs.
pub struct Silent;

impl Progress for Silent {
    fn warning(&self, _label: &str) {}
    fn stage(&self, _label: &str) {}
    fn finish(&self, _label: &str) {}
}

/// A one-shot cancellation flag.
///
/// Deliberately hand-rolled rather than pulling in `tokio-util` for a single
/// type: an abort is one bit, delivered once.
pub mod tokio_util_lite {
    use tokio::sync::watch;

    #[derive(Debug, Clone)]
    pub struct CancelTx(watch::Sender<bool>);

    #[derive(Debug, Clone)]
    pub struct CancelRx(watch::Receiver<bool>);

    pub fn channel() -> (CancelTx, CancelRx) {
        let (tx, rx) = watch::channel(false);
        (CancelTx(tx), CancelRx(rx))
    }

    impl CancelTx {
        pub fn cancel(&self) {
            let _ = self.0.send(true);
        }
    }

    impl CancelRx {
        /// A non-blocking peek, for a loop that wants to check between steps
        /// rather than race a future.
        pub fn is_cancelled(&self) -> bool {
            *self.0.borrow()
        }

        /// Resolves once cancellation is requested, and stays resolved.
        pub async fn cancelled(&mut self) {
            if *self.0.borrow() {
                return;
            }
            while self.0.changed().await.is_ok() {
                if *self.0.borrow() {
                    return;
                }
            }
            std::future::pending::<()>().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_egress_is_the_public_internet() {
        let policy = Policy::default();
        assert!(policy.open, "the guest can reach the public internet");
        assert_eq!(policy.egress_summary(), "internet");
    }

    #[test]
    fn a_closed_policy_with_no_allow_list_has_no_named_hosts() {
        let policy = Policy {
            open: false,
            ..Default::default()
        };
        assert!(policy.egress_hosts().is_empty());
        assert_eq!(policy.egress_summary(), "none");
    }

    #[test]
    fn explicit_hosts_are_sorted_and_deduplicated() {
        let policy = Policy {
            allow_hosts: vec![
                "deb.debian.org".into(),
                "github.com".into(),
                "deb.debian.org".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            policy.egress_hosts(),
            vec!["deb.debian.org".to_string(), "github.com".to_string()]
        );
    }

    #[test]
    fn secret_hosts_join_the_egress_allow_list() {
        let policy = Policy {
            open: false,
            secrets: vec![Secret {
                env: "TOOL_GATEWAY".into(),
                source: "TOOL_GATEWAY".into(),
                placeholder: None,
                hosts: Secret::scoped_hosts(["tool-gateway.shopify.io"]),
            }],
            ..Default::default()
        };
        assert_eq!(
            policy.egress_hosts(),
            vec!["tool-gateway.shopify.io".to_string()]
        );
        assert_eq!(policy.egress_summary(), "tool-gateway.shopify.io");
    }

    #[test]
    fn a_secret_host_with_allow_false_stays_off_the_egress_list() {
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "no.example".into(),
            SecretHost {
                allow: false,
                headers: BTreeMap::new(),
            },
        );
        let policy = Policy {
            open: false,
            secrets: vec![Secret {
                env: "TOKEN".into(),
                source: "TOKEN".into(),
                placeholder: None,
                hosts,
            }],
            ..Default::default()
        };
        assert!(policy.egress_hosts().is_empty());
        assert_eq!(policy.egress_summary(), "none");
    }

    #[test]
    fn secret_hosts_are_a_map_with_allow_and_headers() {
        let secret: Secret = serde_yaml::from_str(
            r#"
env: TOOL_GATEWAY
source: TOOL_GATEWAY
placeholder: reve-tool-gateway
hosts:
  tool-gateway.shopify.io:
    allow: true
    headers:
      Authorization: "Bearer $TOOL_GATEWAY"
"#,
        )
        .unwrap();
        let host = secret.hosts.get("tool-gateway.shopify.io").unwrap();
        assert!(host.allow);
        assert_eq!(
            host.headers.get("Authorization").map(String::as_str),
            Some("Bearer $TOOL_GATEWAY")
        );
        assert_eq!(
            secret.hostnames(),
            vec!["tool-gateway.shopify.io".to_string()]
        );
    }

    #[test]
    fn the_vm_name_is_stable_per_workspace() {
        let policy = Policy::default();
        let a = policy.sandbox_name(Path::new("/tmp/my-agent/workspace"));
        let b = policy.sandbox_name(Path::new("/tmp/my-agent/workspace"));
        let other = policy.sandbox_name(Path::new("/tmp/other-agent/workspace"));
        assert_eq!(a, b, "the same agent restarts the same VM");
        assert_ne!(a, other);
        assert!(a.starts_with("reve-my-agent-"), "got {a}");
    }

    #[test]
    fn an_explicit_name_wins() {
        let policy = Policy {
            name: Some("pinned".into()),
            ..Default::default()
        };
        assert_eq!(policy.sandbox_name(Path::new("/tmp/x/workspace")), "pinned");
    }

    #[test]
    fn runtime_parameters_do_not_change_the_vm_fingerprint() {
        let ws = Path::new("/tmp/agent/workspace");
        let base = Policy::default();
        let mut runtime_changed = base.clone();
        runtime_changed
            .env
            .insert("RUNTIME_FLAG".into(), "different".into());
        runtime_changed.secrets.push(Secret {
            env: "TOKEN".into(),
            source: "HOST_TOKEN".into(),
            placeholder: Some("reve-token".into()),
            hosts: Secret::scoped_hosts(["x.com"]),
        });
        assert_eq!(
            base.fingerprint(ws),
            runtime_changed.fingerprint(ws),
            "exec environment and proxy secrets refresh without rebuilding the VM"
        );
    }

    /// The gateway's 403 body is how the model learns which tool to call;
    /// it must name the real tool and leave `{host}` for microsandbox.
    #[test]
    fn deny_message_names_the_host_permission_tool() {
        let message = Policy::default().http_deny_message();
        assert!(
            message.starts_with("This host is not allowed by the sandbox network policy config.")
        );
        assert!(message.contains("Note to agent:"));
        assert!(message.contains(ASK_HOST_TOOL));
        assert!(message.contains("{host}"));
    }

    #[test]
    fn changing_the_policy_changes_the_fingerprint() {
        let ws = Path::new("/tmp/agent/workspace");
        let base = Policy::default();
        let bigger = Policy {
            cpus: 4,
            ..Policy::default()
        };
        assert_ne!(base.fingerprint(ws), bigger.fingerprint(ws));
    }

    #[test]
    fn the_provision_script_is_guarded_by_its_marker() {
        let script = Policy::default().provision_script();
        assert!(script.contains(PROVISION_MARKER));
        assert!(script.starts_with("set -e"));
        assert!(script.contains("ripgrep"), "default packages are installed");
        assert!(script.contains("node@lts"));
    }

    #[test]
    fn an_unset_scoped_secret_is_reported_before_boot() {
        let secret = Secret {
            env: "GITHUB_TOKEN".into(),
            source: "REVE_TEST_MISSING_GITHUB_TOKEN_9D2B".into(),
            placeholder: Some("reve-github-token".into()),
            hosts: Secret::scoped_hosts(["github.com", "api.github.com"]),
        };
        assert_eq!(
            missing_secret_warning(&secret, Path::new("/tmp")).as_deref(),
            Some(
                "REVE_TEST_MISSING_GITHUB_TOKEN_9D2B is unset; authenticated access for api.github.com, github.com is disabled"
            )
        );
    }

    #[test]
    fn an_unset_github_token_warning_explains_how_to_export_it() {
        let secret = Secret {
            env: "GITHUB_TOKEN".into(),
            source: "GITHUB_TOKEN".into(),
            placeholder: Some("reve-github-token".into()),
            hosts: Secret::scoped_hosts(["github.com", "api.github.com"]),
        };
        assert_eq!(
            format_missing_secret_warning(&secret),
            "GITHUB_TOKEN is unset; authenticated access for api.github.com, github.com is disabled\nexport GITHUB_TOKEN=\"$(gh auth token)\""
        );
    }

    #[test]
    fn an_unset_openrouter_key_warning_explains_how_to_export_it() {
        let secret = Secret {
            env: "OPENROUTER_API_KEY".into(),
            source: "OPENROUTER_API_KEY".into(),
            placeholder: Some("reve-openrouter-key".into()),
            hosts: Secret::scoped_hosts(["openrouter.ai"]),
        };
        assert_eq!(
            format_missing_secret_warning(&secret),
            "OPENROUTER_API_KEY is unset; authenticated access for openrouter.ai is disabled\nexport OPENROUTER_API_KEY=..."
        );
    }

    #[test]
    fn a_command_source_is_the_inner_script() {
        assert_eq!(
            command_secret_source("$(gh auth token)"),
            Some("gh auth token")
        );
        assert_eq!(
            command_secret_source("  $( gh auth token )  "),
            Some("gh auth token")
        );
        assert_eq!(command_secret_source("GITHUB_TOKEN"), None);
        assert_eq!(command_secret_source("$(gh auth token"), None);
        assert_eq!(command_secret_source("$()"), None);
        assert_eq!(command_secret_source("$(echo $(gh auth token))"), None);
    }

    #[test]
    fn a_command_source_runs_on_the_host_and_is_not_an_env_name() {
        let secret = Secret {
            env: "TOKEN_CMD_TEST_9D2B".into(),
            source: "$(/bin/echo command-secret-ok)".into(),
            placeholder: None,
            hosts: Secret::scoped_hosts(["example.com"]),
        };
        let root = Path::new("/tmp");
        assert_eq!(
            secret_value(&secret, root).as_deref(),
            Some("command-secret-ok")
        );
        assert!(missing_secret_warning(&secret, root).is_none());
        let var = bind_secret(&secret, root).expect("bind");
        assert_eq!(var, "REVEBOT_SECRET_TOKEN_CMD_TEST_9D2B");
        assert_eq!(std::env::var(&var).as_deref(), Ok("command-secret-ok"));
    }

    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = HOME_LOCK.lock().unwrap();
        let prev = std::env::var("HOME").ok();
        // SAFETY: test-only HOME swap, restored before return; serialized by HOME_LOCK.
        unsafe { std::env::set_var("HOME", home) };
        let out = f();
        match prev {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    #[test]
    fn expand_tilde_only_rewrites_home() {
        let got = with_home(Path::new("/home/reve"), || {
            (
                expand_tilde("~/cache/token"),
                expand_tilde("~"),
                expand_tilde("~root/x"),
                expand_tilde("/abs"),
            )
        });
        assert_eq!(
            got,
            (
                "/home/reve/cache/token".into(),
                "/home/reve".into(),
                "~root/x".into(),
                "/abs".into()
            )
        );
    }

    #[test]
    fn a_command_source_expands_tilde_in_arguments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "tilde-secret-ok\n").unwrap();
        let secret = Secret {
            env: "TOKEN_TILDE_TEST".into(),
            source: "$(cat ~/token)".into(),
            placeholder: None,
            hosts: Secret::scoped_hosts(["example.com"]),
        };
        let got = with_home(dir.path(), || secret_value(&secret, Path::new("/tmp")));
        assert_eq!(got.as_deref(), Some("tilde-secret-ok"));
    }

    #[test]
    fn a_file_source_expands_a_leading_tilde() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "file-tilde-ok\n").unwrap();
        let secret = Secret {
            env: "TOKEN_FILE_TILDE".into(),
            source: "file:~/token".into(),
            placeholder: None,
            hosts: Secret::scoped_hosts(["example.com"]),
        };
        let got = with_home(dir.path(), || secret_value(&secret, Path::new("/tmp")));
        assert_eq!(got.as_deref(), Some("file-tilde-ok"));
    }

    #[test]
    fn a_failing_command_source_is_reported_not_as_an_unset_env() {
        let secret = Secret {
            env: "TOKEN".into(),
            source: "$(/bin/false)".into(),
            placeholder: None,
            hosts: Secret::scoped_hosts(["example.com"]),
        };
        let root = Path::new("/tmp");
        assert!(secret_value(&secret, root).is_none());
        let warning = missing_secret_warning(&secret, root).expect("warning");
        assert!(warning.starts_with("$(/bin/false) failed"), "{warning}");
        assert!(warning.contains("example.com"), "{warning}");
        assert!(!warning.contains("is unset"), "{warning}");
    }

    #[test]
    fn git_reads_its_token_from_the_environment_not_a_credential_store() {
        assert!(GIT_CREDENTIAL_SETUP.contains("credential.https://github.com.helper"));
        assert!(
            GIT_CREDENTIAL_SETUP.contains("--global"),
            "unprivileged user cannot write git's system config"
        );
        assert!(
            GIT_CREDENTIAL_SETUP.contains("$GITHUB_TOKEN"),
            "the default image has no gh, so the token comes from the environment"
        );
        assert!(
            !GIT_CREDENTIAL_SETUP.contains("store"),
            "and nothing writes it to disk"
        );
    }

    #[test]
    fn the_default_policy_boots_a_preprovisioned_guest() {
        let policy = Policy::default();
        assert_eq!(policy.image, DEFAULT_IMAGE);
        assert!(
            DEFAULT_IMAGE.contains("wrap:desktop"),
            "the default guest is wrap's desktop image, not the CLI-only tag"
        );
        assert!(
            !policy.provision,
            "the image already has the toolchain; installing it again is minutes of nothing"
        );
        assert!(
            policy.root_disk >= 8 * 1024,
            "a rust build tree does not fit in a default rootfs"
        );
        assert!(
            policy.memory >= 8192,
            "XFCE + Chrome + a real build needs more than a headless shell"
        );
        assert!(is_wrap_image(&policy.image));
        assert!(is_desktop_image(&policy.image));
        assert!(!is_wrap_image("alpine"));
        assert!(!is_desktop_image("ghcr.io/tobi/wrap:latest"));
    }

    #[test]
    fn wrap_images_get_a_unix_user_and_desktop_display() {
        let env = guest_unix_env(DEFAULT_IMAGE);
        assert_eq!(env.get("HOME").map(String::as_str), Some(GUEST_HOME));
        assert_eq!(env.get("USER").map(String::as_str), Some(GUEST_USER));
        assert_eq!(
            env.get("DISPLAY").map(String::as_str),
            Some(DESKTOP_DISPLAY)
        );
        assert!(guest_unix_env("alpine").is_empty());
    }

    #[test]
    fn a_novnc_url_points_at_localhost_and_autoconnects() {
        let desktop = Desktop {
            novnc_port: 7608,
            vnc_port: 7590,
        };
        assert_eq!(
            desktop.novnc_url(),
            "http://127.0.0.1:7608/vnc.html?autoconnect=1&resize=scale&reconnect=1"
        );
        assert_eq!(desktop.vnc_addr(), "127.0.0.1:7590");
    }

    #[tokio::test]
    async fn a_cancel_flag_resolves_once_and_stays_resolved() {
        let (tx, mut rx) = tokio_util_lite::channel();
        tx.cancel();
        rx.cancelled().await;
        rx.cancelled().await; // still resolved, does not hang
    }

    #[tokio::test]
    async fn a_rebuild_forgets_the_old_fingerprint_before_replacing_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sandbox-fingerprint");
        std::fs::write(&path, "old-policy\n").unwrap();
        forget_fingerprint(&path).await.unwrap();
        assert!(!path.exists(), "a stale promise must not survive a rebuild");
        forget_fingerprint(&path)
            .await
            .expect("forgetting an absent fingerprint is a no-op");
    }

    #[test]
    fn a_hold_blocks_idle_stop_but_not_a_secret_restart() {
        let mut state = VmState {
            vm: None,
            active: 0,
            holds: 0,
            generation: 0,
            secret_digests: BTreeMap::new(),
        };
        state.hold();
        assert!(
            state.effect_idle(),
            "a hold is not an effect: acquire() may restart for changed secrets"
        );
        state.begin();
        assert!(!state.effect_idle());
        assert_eq!(
            state.finish(),
            None,
            "held: the last effect arms no idle timer"
        );
        let released = state
            .release_hold()
            .expect("the last hold arms the idle timer");
        assert!(state.may_stop(released));
    }

    #[test]
    fn new_activity_invalidates_an_older_idle_deadline() {
        let mut state = VmState {
            vm: None,
            active: 0,
            holds: 0,
            generation: 0,
            secret_digests: BTreeMap::new(),
        };
        state.begin();
        let first_deadline = state.finish().unwrap();
        assert!(state.may_stop(first_deadline));

        state.begin();
        assert!(!state.may_stop(first_deadline));
        let second_deadline = state.finish().unwrap();
        assert!(!state.may_stop(first_deadline));
        assert!(state.may_stop(second_deadline));
    }
}
