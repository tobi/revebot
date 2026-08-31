.PHONY: install eval fmt-check warnings clippy test ci
.NOTPARALLEL: ci

# `--locked` is required: `cargo install` otherwise re-resolves and dies on
# yanked crates that Cargo.lock still pins (chacha20 0.10.x via microsandbox).
# `--offline` skips the registry index (a proxy 403 HTML is not a Cargo error
# you can recover from). `--force` overwrites the same-version binary.
install:
	cargo install --path . --bin revebot --locked --force --offline

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

# Same gate locally and in GitHub Actions. Real microVM tests stay opt-in.
ci: fmt-check warnings clippy test
