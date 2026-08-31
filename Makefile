.PHONY: install eval fmt-check warnings clippy test ci
.NOTPARALLEL: ci

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
# prints coverage counts, and can take an hour.
#
# Either runs on another machine when REMOTE_HOST is set to host:dir, e.g.
#     make spec-full REMOTE_HOST=gb300:~/src/tries/revebot
# The tree (minus target/) is rsynced there first; tla is installed if missing.
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

spec-full:
	$(TLA) docs/tla/DurableLog.tla --config docs/tla/DurableLog.cfg --max-states 50000000
	$(TLA) docs/tla/VmLifecycle.tla --config docs/tla/VmLifecycle.cfg $(SYM_VM) --max-states 50000000 $(COV_VM)
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.small.cfg $(SYM_HARNESS) --max-states 50000000 $(COV_HARNESS)
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.cfg $(SYM_HARNESS) --max-states 200000000 $(COV_HARNESS)
else
REMOTE_SSH := $(word 1,$(subst :, ,$(REMOTE_HOST)))
REMOTE_DIR := $(word 2,$(subst :, ,$(REMOTE_HOST)))
spec spec-full:
	ssh $(REMOTE_SSH) 'mkdir -p $(REMOTE_DIR)'
	rsync -az --delete --exclude target --exclude .git ./ $(REMOTE_HOST)/
	ssh $(REMOTE_SSH) 'cd $(REMOTE_DIR) && export PATH="$$HOME/.cargo/bin:$$PATH" && \
	  (command -v tla >/dev/null || cargo install tla-checker@0.6.11 --bin tla) && \
	  make $@ TLA=tla'
endif

# Same gate locally and in GitHub Actions. Real microVM tests stay opt-in.
ci: fmt-check warnings clippy test spec
