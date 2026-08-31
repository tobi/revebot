.PHONY: install eval

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
	cargo run --bin revebot -- eval $(ARGS)
