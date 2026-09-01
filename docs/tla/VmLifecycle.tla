---- MODULE VmLifecycle ----
\* One microVM, many bots — src/sandbox.rs `Sandbox` + src/house/mod.rs.
\*
\* The VM is the only place a tool effect can run (AGENTS.md: "Sandbox or no
\* Reve"). It has its own lifecycle, separate from any bot's operation:
\* built from a policy (config.yml) and remembered by a fingerprint file;
\* started on the first effect; idle-stopped after the last; pinned by the
\* house's hold for the process lifetime; carrying a secret set that is only
\* fully re-read at (re)start. This module checks that lifecycle against the
\* updates that arrive while bots are mid-effect: a policy edit, a host
\* secret rotation, an AskUserForSecret save, a house crash, a house restart.
\*
\* "Effective" state is what the running guest actually has. "Defined" state
\* is what the persisted microsandbox definition will have at its next start.
\* "Desired" is the host truth (config.yml + environment). The point of the
\* module is the gaps between those three.
EXTENDS Naturals, FiniteSets

CONSTANTS
    Bots,
    Policies,     \* distinct config.yml contents -> distinct fingerprints
    MaxSecretV,   \* secret set versions 0..MaxSecretV
    MaxSteps      \* bound on the number of transitions

None == "none"
Versions == 0..MaxSecretV
VmStatuses == {"absent", "stopped", "running"}
HouseStates == {"down", "up"}
BotStates == {"idle", "executing"}

VARIABLES
    t,              \* step counter (model bound)
    \* host truth
    policy,         \* Policies: current config.yml
    desired,        \* Versions: current secret set on the host
    \* microsandbox definition + instance
    vmStatus,       \* VmStatuses
    vmDisk,         \* {None} \cup Policies: policy the root disk was built from
    vmProvisioned,  \* BOOLEAN: build's provisioning/bootstrap finished ok
    vmDefined,      \* {None} \cup Versions: secrets in the persisted definition
    vmEffective,    \* {None} \cup Versions: secrets live in the running guest
    vmDefinitionUsable, \* BOOLEAN: persisted ports/policy can provide the configured desktop
    \* .reve/sandbox-fingerprint
    fingerprint,    \* {None} \cup Policies
    \* the Reve process
    house,          \* HouseStates
    bootedPolicy,   \* {None} \cup Policies: config.yml as read by this house at boot
    held,           \* BOOLEAN: house hold() taken (VmState.holds > 0)
    active,         \* 0..Cardinality(Bots): VmState.active — live effects only
    digests,        \* {None} \cup Versions: VmState.secret_digests bookkeeping
    idleArmed,      \* BOOLEAN: a release() spawned the idle timer
    bots,           \* [Bots -> BotStates]
    \* ghosts
    staleAcquire    \* BOOLEAN: an effect started against a guest whose secret
                    \* set was not the host's at that moment

vars == <<t, policy, desired, vmStatus, vmDisk, vmProvisioned, vmDefined,
          vmEffective, vmDefinitionUsable, fingerprint, house, bootedPolicy, held,
          active, digests, idleArmed, bots, staleAcquire>>

Running == vmStatus = "running"
Up == house = "up"
Tick == t' = t + 1
Bounded == t < MaxSteps

-----------------------------------------------------------------------------

Init ==
    /\ t = 0
    /\ policy \in Policies
    /\ desired = 0
    /\ vmStatus = "absent"
    /\ vmDisk = None
    /\ vmProvisioned = FALSE
    /\ vmDefined = None
    /\ vmEffective = None
    /\ vmDefinitionUsable = FALSE
    /\ fingerprint = None
    /\ house = "down"
    /\ bootedPolicy = None
    /\ held = FALSE
    /\ active = 0
    /\ digests = None
    /\ idleArmed = FALSE
    /\ bots = [b \in Bots |-> "idle"]
    /\ staleAcquire = FALSE

-----------------------------------------------------------------------------
\* Host updates. They can happen at any time, house up or down.

EditPolicy(p) ==
    /\ Bounded /\ Tick
    /\ p # policy
    /\ policy' = p
    /\ UNCHANGED <<desired, vmStatus, vmDisk, vmProvisioned, vmDefined, vmEffective,
                   vmDefinitionUsable, fingerprint, house, bootedPolicy, held, active,
                   digests, idleArmed, bots, staleAcquire>>

\* A `$` host-environment source or another dynamic source such as `$(command)`
\* changed. Reve notices only through runtime_secret_digests at acquire.
RotateHostSecret ==
    /\ Bounded /\ Tick
    /\ desired < MaxSecretV
    /\ desired' = desired + 1
    /\ UNCHANGED <<policy, vmStatus, vmDisk, vmProvisioned, vmDefined, vmEffective,
                   vmDefinitionUsable, fingerprint, house, bootedPolicy, held, active,
                   digests, idleArmed, bots, staleAcquire>>

-----------------------------------------------------------------------------
\* microsandbox stop of the running instance. Start reads the definition's
\* secret sources at that moment (effective := defined); it is inlined where
\* it happens because each caller also sets the definition first.
StopVm ==
    /\ vmStatus' = "stopped"
    /\ vmEffective' = None

\* A persisted definition from an older Reve may have the right policy
\* fingerprint but a network policy that denies its published desktop ports.
LegacyIncompatibleDefinition ==
    /\ Bounded /\ Tick
    /\ house = "down"
    /\ vmStatus = "absent"
    /\ vmStatus' = "stopped"
    /\ vmDisk' = policy
    /\ vmProvisioned' = TRUE
    /\ vmDefined' = desired
    /\ vmEffective' = None
    /\ vmDefinitionUsable' = FALSE
    /\ fingerprint' = policy
    /\ UNCHANGED <<policy, desired, house, bootedPolicy, held, active, digests,
                   idleArmed, bots, staleAcquire>>

\* Process boot: reclaim a namesake left running by a dead process, start
\* (reuse or build). The house then hold()s for its lifetime; `revebot exec`
\* and the TUI do not, so their guest idle-stops between effects.
HouseBoot(ok, hold) ==
    /\ Bounded /\ Tick
    /\ house = "down"
    /\ house' = "up"
    /\ bootedPolicy' = policy
    /\ held' = hold
    /\ active' = 0
    /\ idleArmed' = FALSE
    /\ bots' = [b \in Bots |-> "idle"]
    /\ LET afterReclaim == IF vmStatus = "running" THEN "stopped" ELSE vmStatus
       IN \/ /\ afterReclaim = "stopped"
             /\ fingerprint = policy
             /\ vmDefinitionUsable
             /\ vmDefinitionUsable' = vmDefinitionUsable
             /\ vmDefined' = desired
             /\ vmStatus' = "running"
             /\ vmEffective' = desired
             /\ digests' = desired
             /\ UNCHANGED <<vmDisk, vmProvisioned, fingerprint>>
          \/ /\ ~(afterReclaim = "stopped" /\ fingerprint = policy /\ vmDefinitionUsable)
             /\ vmDefinitionUsable' = TRUE
             /\ vmStatus' = "running"
             /\ vmDisk' = policy
             /\ vmProvisioned' = ok
             /\ vmDefined' = desired
             /\ vmEffective' = desired
             /\ digests' = desired
             \* forget_fingerprint before build: a failed provisioning leaves
             \* no promise behind, never a stale one
             /\ fingerprint' = IF ok THEN policy ELSE None
    /\ UNCHANGED <<policy, desired, staleAcquire>>

\* A crash mid-build: the old fingerprint was forgotten, the definition was
\* replaced, the new fingerprint was not written, the process is gone.
HouseCrashMidBuild ==
    /\ Bounded /\ Tick
    /\ house = "down"
    /\ ~(vmStatus # "absent" /\ fingerprint = policy)
    /\ vmStatus' = "stopped"
    /\ vmDisk' = policy
    /\ vmProvisioned' = FALSE
    /\ vmDefined' = desired
    /\ vmEffective' = None
    /\ fingerprint' = None
    /\ vmDefinitionUsable' = TRUE
    /\ UNCHANGED <<policy, desired, house, bootedPolicy, held, active,
                   digests, idleArmed, bots, staleAcquire>>

\* Graceful shutdown: release_hold, then stop.
HouseStop ==
    /\ Bounded /\ Tick
    /\ Up
    /\ house' = "down"
    /\ held' = FALSE
    /\ active' = 0
    /\ digests' = None
    /\ idleArmed' = FALSE
    /\ bots' = [b \in Bots |-> "idle"]
    /\ IF Running THEN StopVm ELSE UNCHANGED <<vmStatus, vmEffective>>
    /\ UNCHANGED <<policy, desired, vmDisk, vmProvisioned, vmDefined,
                   vmDefinitionUsable, fingerprint, bootedPolicy, staleAcquire>>

\* The process dies. The VM keeps running in microsandbox (reclaimed at the
\* next boot). Every live effect dies with the process.
HouseCrash ==
    /\ Bounded /\ Tick
    /\ Up
    /\ house' = "down"
    /\ held' = FALSE
    /\ active' = 0
    /\ digests' = None
    /\ idleArmed' = FALSE
    /\ bots' = [b \in Bots |-> "idle"]
    /\ UNCHANGED <<policy, desired, vmStatus, vmDisk, vmProvisioned, vmDefined,
                   vmEffective, vmDefinitionUsable, fingerprint, bootedPolicy,
                   staleAcquire>>

-----------------------------------------------------------------------------
\* Effects. Sandbox::acquire / release, one per ctx.sh / exec / file op.

\* acquire(): start on demand; restart when idle and the host secret digest
\* moved; otherwise use the running guest as-is. Then active += 1.
Acquire(b) ==
    /\ Bounded /\ Tick
    /\ Up
    /\ bots[b] = "idle"
    /\ bots' = [bots EXCEPT ![b] = "executing"]
    /\ active' = active + 1
    /\ idleArmed' = FALSE
    /\ \/ \* no live handle: install sources on the definition, start
          /\ vmStatus = "stopped"
          /\ vmDefined' = desired
          /\ vmStatus' = "running"
          /\ vmEffective' = desired
          /\ digests' = desired
          /\ staleAcquire' = staleAcquire
       \/ \* idle and the digest moved: stop, reinstall, start
          /\ Running
          /\ active = 0
          /\ digests # desired
          /\ vmDefined' = desired
          /\ vmStatus' = "running"
          /\ vmEffective' = desired
          /\ digests' = desired
          /\ staleAcquire' = staleAcquire
       \/ \* running and (busy or digest unchanged): use it
          /\ Running
          /\ ~(active = 0 /\ digests # desired)
          /\ UNCHANGED <<vmStatus, vmDefined, vmEffective, digests>>
          /\ staleAcquire' = (staleAcquire \/ (active = 0 /\ vmEffective # desired))
    /\ UNCHANGED <<policy, desired, vmDisk, vmProvisioned, vmDefinitionUsable,
                   fingerprint, house, bootedPolicy, held>>

\* release(): active -= 1; the last one arms the idle timer.
Release(b) ==
    /\ Bounded /\ Tick
    /\ Up
    /\ bots[b] = "executing"
    /\ bots' = [bots EXCEPT ![b] = "idle"]
    /\ active' = active - 1
    /\ idleArmed' = (active - 1 = 0 /\ ~held)
    /\ UNCHANGED <<policy, desired, vmStatus, vmDisk, vmProvisioned, vmDefined,
                   vmEffective, vmDefinitionUsable, fingerprint, house, bootedPolicy,
                   held, digests, staleAcquire>>

\* The idle timer fires: stop only if still idle (may_stop). Any acquire in
\* between disarmed it (generation moved).
IdleStop ==
    /\ Bounded /\ Tick
    /\ Up
    /\ idleArmed
    /\ active = 0
    /\ ~held
    /\ idleArmed' = FALSE
    /\ digests' = None
    /\ IF Running THEN StopVm ELSE UNCHANGED <<vmStatus, vmEffective>>
    /\ UNCHANGED <<policy, desired, vmDisk, vmProvisioned, vmDefined,
                   vmDefinitionUsable, fingerprint, house, bootedPolicy, held,
                   active, bots, staleAcquire>>

\* AskUserForSecret accepted: config.yml updated (desired moves), then
\* Sandbox::upsert_secret reinstalls the definition with next_start and
\* records the new digest. The running guest is untouched.
UpsertSecret ==
    /\ Bounded /\ Tick
    /\ Up
    /\ desired < MaxSecretV
    /\ desired' = desired + 1
    /\ vmDefined' = IF vmStatus = "absent" THEN vmDefined ELSE desired + 1
    \* secret_digests keeps describing the running guest; the next effect-idle
    \* acquire sees the difference and restarts
    /\ UNCHANGED <<policy, vmStatus, vmDisk, vmProvisioned, vmEffective,
                   vmDefinitionUsable, fingerprint, house, bootedPolicy, held,
                   active, digests, idleArmed, bots, staleAcquire>>

Next ==
    \/ \E p \in Policies: EditPolicy(p)
    \/ RotateHostSecret
    \/ LegacyIncompatibleDefinition
    \/ \E ok, hold \in BOOLEAN: HouseBoot(ok, hold)
    \/ HouseCrashMidBuild
    \/ HouseStop
    \/ HouseCrash
    \/ \E b \in Bots: Acquire(b)
    \/ \E b \in Bots: Release(b)
    \/ IdleStop
    \/ UpsertSecret

-----------------------------------------------------------------------------

TypeOK ==
    /\ t \in 0..MaxSteps
    /\ policy \in Policies
    /\ desired \in Versions
    /\ vmStatus \in VmStatuses
    /\ vmDisk \in {None} \cup Policies
    /\ vmProvisioned \in BOOLEAN
    /\ vmDefined \in {None} \cup Versions
    /\ vmEffective \in {None} \cup Versions
    /\ vmDefinitionUsable \in BOOLEAN
    /\ fingerprint \in {None} \cup Policies
    /\ house \in HouseStates
    /\ bootedPolicy \in {None} \cup Policies
    /\ held \in BOOLEAN
    /\ active \in 0..Cardinality(Bots)
    /\ digests \in {None} \cup Versions
    /\ idleArmed \in BOOLEAN
    /\ bots \in [Bots -> BotStates]
    /\ staleAcquire \in BOOLEAN

\* An effect only ever runs against a running guest.
InvEffectNeedsRunningVm ==
    active > 0 => Running

\* A hold keeps the guest up: no idle stop while the house holds it.
InvHeldVmStaysUp ==
    Up /\ held => Running

\* The house never runs the VM without having started it in this process
\* (a namesake left running by a dead house is reclaimed, never adopted).
InvNoAdoptedVm ==
    house = "down" => active = 0

\* The fingerprint file is a promise about the disk: if it names the current
\* policy, the disk really was built from that policy and provisioned.
InvFingerprintHonest ==
    fingerprint # None => (vmDisk = fingerprint /\ vmProvisioned)

\* What the house runs is what config.yml said when this house booted. (A
\* later edit takes effect at the next boot; there is no hot reload.)
InvRunningDiskMatchesBootPolicy ==
    Up /\ Running => vmDisk = bootedPolicy

\* A running desktop definition must admit its published localhost ports.
InvRunningDefinitionUsable ==
    Up /\ Running => vmDefinitionUsable

\* The digest bookkeeping describes the running guest, not the definition.
InvDigestsDescribeGuest ==
    Running /\ digests # None => digests = vmEffective

\* Secret updates land at the next effect-idle boundary: an effect that
\* starts while no other effect is live always sees the host's current
\* secret set. (An effect that starts while another is live shares that
\* guest and may see the previous set — recorded in docs/tla/README.md.)
InvIdleAcquireIsFresh ==
    ~staleAcquire

\* Coverage probes.
CovRebuildAfterPolicyEdit == Up /\ Running /\ vmDisk = policy /\ fingerprint # None
CovLegacyIncompatibleDefinition ==
    house = "down" /\ vmStatus = "stopped" /\ ~vmDefinitionUsable
CovIdleStopped == Up /\ ~Running /\ ~held
CovTwoBotsExecuting == \A b \in Bots: bots[b] = "executing"
CovUpsertWhileRunning == Running /\ vmDefined # vmEffective
CovRestartForSecrets == Up /\ Running /\ vmEffective = desired /\ desired > 0
CovStaleUnderConcurrency ==
    \E b \in Bots: bots[b] = "executing" /\ Running /\ vmEffective # desired

====
