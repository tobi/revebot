.PHONY: install eval fmt-check warnings clippy test tla tla-deep ci
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

# Model-check the durable log, the lane/inbox state machine (docs/harness.md)
# and the shared microVM lifecycle (src/sandbox.rs) against their invariants. Needs `tla` (cargo install tla-checker --bin tla).
# The small harness configuration is the CI size; `tla-deep` is opt-in.
TLA ?= tla
tla:
	$(TLA) docs/tla/DurableLog.tla --config docs/tla/DurableLog.cfg --max-states 3000000
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.small.cfg -s EntryIds -s OpIds -s Lanes --max-states 3000000
	$(TLA) docs/tla/VmLifecycle.tla --config docs/tla/VmLifecycle.cfg -s Bots -s Policies --max-states 3000000

tla-deep:
	$(TLA) docs/tla/DurableHarness.tla --config docs/tla/DurableHarness.cfg -s EntryIds -s OpIds -s Lanes --max-states 20000000

# Same gate locally and in GitHub Actions. Real microVM tests stay opt-in.
ci: fmt-check warnings clippy test tla
