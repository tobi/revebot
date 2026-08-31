.PHONY: install eval fmt-check warnings clippy test ci

# Honor an inherited rustup selection explicitly instead of letting cargo
# install warn about an implicit toolchain override. An unset selection keeps
# cargo's normal default; INSTALL_TOOLCHAIN can also be supplied to make.
INSTALL_TOOLCHAIN ?= $(RUSTUP_TOOLCHAIN)

# Keep the reviewed dependency graph and use the local cache. Installing only
# revebot keeps test-helper binaries out of ~/.cargo/bin.
install:
	cargo $(if $(strip $(INSTALL_TOOLCHAIN)),+$(INSTALL_TOOLCHAIN)) install --path . --bin revebot --locked --force --offline

# Offline eval catalog. Extra flags: `make eval ARGS='--live'`.
# Live cases use OPENROUTER_API_KEY (openrouter/x-ai/grok-4.6) unless
# REVEBOT_EVAL_MODEL is set.
eval:
	cargo run --locked --bin revebot -- eval $(ARGS)

fmt-check:
	cargo fmt --all --check

# Do not rely on Clippy alone to catch rustc warnings.
warnings:
	RUSTFLAGS="$(RUSTFLAGS) -D warnings" cargo check --locked --all-targets

# Cargo.toml is the policy; clippy.toml contains its narrow test exemptions.
clippy:
	cargo clippy --locked --all-targets -- -D warnings

test:
	cargo test --locked

# Model-check docs/tla — the durable log, the lane/inbox/tool state machine
# (docs/harness.md) and the shared microVM lifecycle (src/sandbox.rs) — with
# tla-rs (`cargo install tla-checker --bin tla`). `spec` is the CI size and is
# what `make ci` runs (`tla` is its alias); `spec-full` uses the deep bounds and
# prints coverage counts, and can take hours; `make -j spec-full` parallelises.
#
# Either runs on another machine when REMOTE_HOST is set to host:dir, e.g.
#     make spec-full REMOTE_HOST=gb300:~/src/tries/revebot
# The tree (minus target/) is rsynced there first, tla is installed if missing,
# and the run lives in a named herdr workspace there (`herdr --remote gb300`).
TLA ?= tla
SYM_HARNESS := -s EntryIds -s OpIds -s Lanes
SYM_VM := -s Bots -s Policies
COV_HARNESS := --count-satisfying CovBatchCompleted --count-satisfying CovTwoToolsLive \
	--count-satisfying CovLaterCallSettledFirst --count-satisfying CovInterruptedNeverSynthesized \
	--count-satisfying CovBothLanesOpen
COV_VM := --count-satisfying CovRebuildAfterPolicyEdit --count-satisfying CovIdleStopped \
	--count-satisfying CovTwoBotsExecuting --count-satisfying CovUpsertWhileRunning \
	--count-satisfying CovRestartForSecrets --count-satisfying CovStaleUnderConcurrency
REMOTE_HOST ?=

.PHONY: spec spec-full tla
tla: spec

ifeq ($(strip $(REMOTE_HOST)),)
spec:
	$(TLA) docs/tla/DurableLog.tla --config docs/tla/DurableLog.cfg --max-states 3000000
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.small.cfg $(SYM_HARNESS) --max-states 3000000
	$(TLA) docs/tla/VmLifecycle.tla --config docs/tla/VmLifecycle.cfg $(SYM_VM) --max-states 3000000

# Four independent checks; `make -j spec-full` runs them all at once (the
# checker is single-threaded). Each writes target/spec-<name>.log.
spec-full: spec-full-log spec-full-vm spec-full-harness spec-full-harness-six spec-full-harness-deep
	@for f in log vm harness harness-six harness-deep; do echo "== $$f"; grep -E "Reachable states|Time:|Cov|violated|error" target/spec-$$f.log; done

.PHONY: spec-full-log spec-full-vm spec-full-harness spec-full-harness-six spec-full-harness-deep
spec-full-log:
	@mkdir -p target
	$(TLA) docs/tla/DurableLog.tla --config docs/tla/DurableLog.cfg --max-states 50000000 > target/spec-log.log 2>&1
spec-full-vm:
	@mkdir -p target
	$(TLA) docs/tla/VmLifecycle.tla --config docs/tla/VmLifecycle.cfg $(SYM_VM) --max-states 50000000 $(COV_VM) > target/spec-vm.log 2>&1
spec-full-harness:
	@mkdir -p target
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.small.cfg $(SYM_HARNESS) --max-states 50000000 $(COV_HARNESS) > target/spec-harness.log 2>&1
# CI-sizing candidate: the small configuration one commit shorter.
spec-full-harness-six:
	@mkdir -p target
	sed 's/MaxSeq = 7/MaxSeq = 6/' docs/tla/DurableHarness.small.cfg > target/DurableHarness.six.cfg
	$(TLA) docs/tla/DurableHarness.tla --config target/DurableHarness.six.cfg $(SYM_HARNESS) --max-states 50000000 $(COV_HARNESS) > target/spec-harness-six.log 2>&1
spec-full-harness-deep:
	@mkdir -p target
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.cfg $(SYM_HARNESS) --max-states 200000000 $(COV_HARNESS) > target/spec-harness-deep.log 2>&1
else
# scripts/spec-remote.sh rsyncs the tree, then runs the target inside a herdr
# workspace labelled REMOTE_LABEL on the remote's herdr server (watch it with
# `herdr --remote <host>`); without a remote herdr server it runs over plain ssh.
REMOTE_LABEL ?= revebot-spec
spec spec-full:
	scripts/spec-remote.sh '$(REMOTE_HOST)' $@ '$(REMOTE_LABEL)'
endif

# Same gate locally and in GitHub Actions. Real microVM tests stay opt-in. The
# steps run in this order even under -j (a `.NOTPARALLEL: ci` would disable
# parallelism for every target on GNU make < 4.4).
ci:
	$(MAKE) fmt-check
	$(MAKE) warnings
	$(MAKE) clippy
	$(MAKE) test
	$(MAKE) spec
