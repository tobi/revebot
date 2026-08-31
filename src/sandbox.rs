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

/// The default guest: an Arch image that already contains the toolchain an
/// agent reaches for, with mise pinned to absolute paths under `/opt` so the
/// shims work whatever `HOME` ends up being.
///
/// Baking the toolchain into the image rather than installing it on first boot
/// is the difference between a cold start of seconds and one of minutes, and it
/// removes the failure mode where the agent's first turn depends on a package
/// mirror being up. It is also why [`Policy::provision`] defaults to `false`:
/// there is nothing left to provision.
pub const DEFAULT_IMAGE: &str = "ghcr.io/tobi/wrap:latest";

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
/// The old form shelled out to `gh auth git-credential`, which the default
/// image does not contain. A store-free helper is better anyway: the token is
/// already in the guest environment as a microsandbox-resolved secret, so this
/// writes no credential file and leaves nothing behind on the disk.
const GIT_CREDENTIAL_SETUP: &str = "if command -v git >/dev/null; then \
git config --system credential.https://github.com.helper \
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
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Optional HTTP header the guest should send the placeholder in
    /// (e.g. `Authorization`). The proxy substitutes the real value for
    /// `hosts` only — the guest never sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Optional prefix in that header, typically `Bearer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
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
            memory: 2048,
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
    /// Hosts named in `allow`. When [`Self::open`] is true the guest can
    /// already reach the public internet; this list is extra (or the whole
    /// allowlist when `open` is false).
    pub fn egress_hosts(&self) -> Vec<String> {
        let mut hosts = self.allow_hosts.clone();
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
    vm: Arc<Mutex<VmState>>,
}

struct VmState {
    vm: Option<MsbSandbox>,
    active: usize,
    generation: u64,
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
        (self.active == 0).then_some(self.generation)
    }

    fn may_stop(&self, generation: u64) -> bool {
        self.active == 0 && self.generation == generation
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
                let secret_digests = runtime_secret_digests(&policy.secrets, &secret_root);
                let sandbox = Self {
                    secrets: parking_lot::Mutex::new(policy.secrets.clone()),
                    secret_root,
                    policy,
                    host_workspace,
                    name,
                    vm: Arc::new(Mutex::new(VmState {
                        vm: Some(vm),
                        active: 0,
                        generation: 0,
                        secret_digests,
                    })),
                };
                sandbox.configure_git_credentials().await;
                progress.finish("sandbox ready");
                return Ok(sandbox);
            }
        }

        progress.stage(&format!("building microVM {name} from {}", policy.image));
        let vm = build(&policy, &name, &host_workspace, &secret_root).await?;
        let secret_digests = runtime_secret_digests(&policy.secrets, &secret_root);
        let sandbox = Self {
            secrets: parking_lot::Mutex::new(policy.secrets.clone()),
            secret_root,
            policy,
            host_workspace,
            name,
            vm: Arc::new(Mutex::new(VmState {
                vm: Some(vm),
                active: 0,
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
        sandbox.configure_git_credentials().await;
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

    /// Keep the guest from idle-stopping for the lifetime of the house.
    pub async fn hold(&self) -> Result<()> {
        let _ = self.acquire().await?;
        Ok(())
    }

    /// Pair of [`hold`]: allow idle-stop again.
    pub async fn release_hold(&self) {
        self.release().await;
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
            let mut env = self.policy.env.clone();
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
        let mut state = self.vm.lock().await;
        if let Some(vm) = state.vm.as_ref() {
            let config = vm.config();
            let existing = persisted_secret_names(config);
            remove_secret_definitions(vm.modify(), &existing, true).await?;
            install_secret_definitions(vm.modify(), &secrets, true, &self.secret_root).await?;
            state.secret_digests = runtime_secret_digests(&secrets, &self.secret_root);
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
        } else if state.active == 0 && state.secret_digests != desired {
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
        let Some(generation) = generation else {
            return;
        };
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

    async fn configure_git_credentials(&self) {
        let options = ExecOptions {
            cwd: Some("/".into()),
            ..Default::default()
        };
        let _ = self.exec(GIT_CREDENTIAL_SETUP, options, None).await;
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

fn run_secret_command(script: &str) -> Result<String, String> {
    let words = shell_words::split(script).map_err(|e| e.to_string())?;
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
    let path = Path::new(rest.trim());
    if rest.trim().is_empty() {
        return None;
    }
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
            for host in &secret.hosts {
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
    let hosts = secret.hosts.join(", ");
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

/// Turn a [`Policy`] into a booted VM.
async fn build(
    policy: &Policy,
    name: &str,
    host_workspace: &Path,
    secret_root: &Path,
) -> Result<MsbSandbox> {
    let mut builder = MsbSandbox::builder(name.to_string())
        .image(policy.image.clone())
        .root_disk(policy.root_disk.mib())
        .cpus(policy.cpus)
        .memory(policy.memory)
        .workdir(policy.workdir.clone())
        .replace();

    // Ordinary environment values are exec-time parameters, not VM state.

    if policy.mount_workspace {
        let host = host_workspace.to_path_buf();
        builder = builder.volume(policy.workdir.clone(), move |m| m.bind(host));
    }

    // Open (the default): public internet + gateway DNS.
    // Locked down: deny both directions, gateway DNS, then named hosts.
    let mut network = if policy.open {
        NetworkPolicy::from_profiles([NetworkProfile::Public])
    } else {
        let mut network = NetworkPolicy::none();
        network.rules.push(Rule::allow_dns());
        network
    };
    if !policy.allow_hosts.is_empty() {
        network = network
            .allow_domains(policy.egress_hosts())
            .map_err(|e| SandboxError::Policy(format!("invalid allowed host: {e}")))?;
    }
    builder = builder.network(move |n| n.enabled(true).policy(network));

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
            for host in &secret.hosts {
                entry = entry.allow_host(host.as_str());
            }
            entry
        });
    }
    builder
        .create()
        .await
        .map_err(|e| SandboxError::Unavailable(e.to_string()))
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
            hosts: vec!["x.com".into()],
            ..Default::default()
        });
        assert_eq!(
            base.fingerprint(ws),
            runtime_changed.fingerprint(ws),
            "exec environment and proxy secrets refresh without rebuilding the VM"
        );
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
            hosts: vec!["github.com".into(), "api.github.com".into()],
            ..Default::default()
        };
        assert_eq!(
            missing_secret_warning(&secret, Path::new("/tmp")).as_deref(),
            Some(
                "REVE_TEST_MISSING_GITHUB_TOKEN_9D2B is unset; authenticated access for github.com, api.github.com is disabled"
            )
        );
    }

    #[test]
    fn an_unset_github_token_warning_explains_how_to_export_it() {
        let secret = Secret {
            env: "GITHUB_TOKEN".into(),
            source: "GITHUB_TOKEN".into(),
            placeholder: Some("reve-github-token".into()),
            hosts: vec!["github.com".into(), "api.github.com".into()],
            ..Default::default()
        };
        assert_eq!(
            format_missing_secret_warning(&secret),
            "GITHUB_TOKEN is unset; authenticated access for github.com, api.github.com is disabled\nexport GITHUB_TOKEN=\"$(gh auth token)\""
        );
    }

    #[test]
    fn an_unset_openrouter_key_warning_explains_how_to_export_it() {
        let secret = Secret {
            env: "OPENROUTER_API_KEY".into(),
            source: "OPENROUTER_API_KEY".into(),
            placeholder: Some("reve-openrouter-key".into()),
            hosts: vec!["openrouter.ai".into()],
            ..Default::default()
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
            hosts: vec!["example.com".into()],
            ..Default::default()
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

    #[test]
    fn a_failing_command_source_is_reported_not_as_an_unset_env() {
        let secret = Secret {
            env: "TOKEN".into(),
            source: "$(/bin/false)".into(),
            placeholder: None,
            hosts: vec!["example.com".into()],
            ..Default::default()
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
            !policy.provision,
            "the image already has the toolchain; installing it again is minutes of nothing"
        );
        assert!(
            policy.root_disk >= 8 * 1024,
            "a rust build tree does not fit in a default rootfs"
        );
    }

    #[tokio::test]
    async fn a_cancel_flag_resolves_once_and_stays_resolved() {
        let (tx, mut rx) = tokio_util_lite::channel();
        tx.cancel();
        rx.cancelled().await;
        rx.cancelled().await; // still resolved, does not hang
    }

    #[test]
    fn new_activity_invalidates_an_older_idle_deadline() {
        let mut state = VmState {
            vm: None,
            active: 0,
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
