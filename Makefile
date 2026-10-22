# Common commands. `make` alone lists targets.
#
# There is deliberately no live-trading target: live needs the
# `--i-understand` flag typed by a person on each run (CLAUDE.md invariant 7),
# and a make target would bake that lock away. Run live with cargo directly.

DATABASE_URL ?= sqlite://bot.db?mode=rwc
SYMBOL       ?= BTCUSDT
INTERVAL     ?= 1h
FROM         ?= 2024-01-01
TO           ?= 2024-06-01
STRATEGY     ?= ratio_hop

CLI := cargo run -p cli --

.DEFAULT_GOAL := help
.PHONY: help build test lint fmt fmt-check doc check \
        migrate-up migrate-down migrate-status migrate-fresh \
        fetch-data backtest run-paper clean

help: ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*## "} {printf "  %-15s %s\n", $$1, $$2}'

## --- Develop -------------------------------------------------------------

build: ## Build every crate
	cargo build --workspace

test: ## Run all tests
	cargo test --workspace

lint: ## Clippy with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

fmt: ## Format code
	cargo fmt --all

fmt-check: ## Fail if code is not formatted
	cargo fmt --all --check

doc: ## Build docs, failing on broken links or doc warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

check: fmt-check lint doc test ## Definition-of-done gate: run before every commit

## --- Database (override DATABASE_URL=...) --------------------------------

migrate-up: ## Apply pending migrations
	DATABASE_URL='$(DATABASE_URL)' sea-orm-cli migrate up -d migration

migrate-down: ## Roll back the last migration
	DATABASE_URL='$(DATABASE_URL)' sea-orm-cli migrate down -d migration

migrate-status: ## Show applied and pending migrations
	DATABASE_URL='$(DATABASE_URL)' sea-orm-cli migrate status -d migration

migrate-fresh: ## DROP ALL TABLES, then re-apply every migration (dev only)
	DATABASE_URL='$(DATABASE_URL)' sea-orm-cli migrate fresh -d migration

## --- Bot (override SYMBOL, INTERVAL, FROM, TO, STRATEGY) -----------------

fetch-data: ## Cache klines into data/
	$(CLI) fetch-data --symbol $(SYMBOL) --interval $(INTERVAL) --from $(FROM)

backtest: ## Backtest STRATEGY over FROM..TO
	$(CLI) backtest --strategy $(STRATEGY) --from $(FROM) --to $(TO)

run-paper: ## Paper trade against testnet prices (the safe default)
	$(CLI) run --paper

clean: ## Remove build output
	cargo clean
