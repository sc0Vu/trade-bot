# Common commands. `make` alone lists targets.
#
# There is deliberately no live-trading target: live needs the
# `--i-understand` flag typed by a person on each run (CLAUDE.md invariant 7),
# and a make target would bake that lock away. Run live with cargo directly.

DATABASE_URL ?= sqlite://bot.db?mode=rwc
VENUE        ?= binance
SYMBOL       ?= BTCUSDT
INTERVAL     ?= 1h
FROM         ?= 2024-01-01
TO           ?= 2024-06-01
STRATEGY     ?= ratio_hop
# Taker slippage for backtests, in bps. No default on purpose: set it.
SLIPPAGE_BPS ?=
# Optional backtest overrides: fee rate (default 0.001) and starting cash
# (default 1000, in the symbol's quote asset).
FEE          ?=
INITIAL_CASH ?=

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

## --- Bot (override SYMBOL, INTERVAL, FROM, TO, STRATEGY, PARAMS, SLIPPAGE_BPS)

fetch-data: ## Cache klines into data/ (VENUE=binance|max)
	$(CLI) fetch-data --venue $(VENUE) --symbol $(SYMBOL) --interval $(INTERVAL) --from $(FROM)

backtest: ## Backtest STRATEGY on VENUE's SYMBOL over FROM..TO (SLIPPAGE_BPS required; PARAMS, FEE, INITIAL_CASH)
	@test -n "$(SLIPPAGE_BPS)" || { echo "set SLIPPAGE_BPS, e.g. make backtest SLIPPAGE_BPS=5"; exit 2; }
	$(CLI) backtest --venue $(VENUE) --strategy $(STRATEGY) --symbol $(SYMBOL) --interval $(INTERVAL) \
		--from $(FROM) --to $(TO) --slippage-bps $(SLIPPAGE_BPS) $(if $(PARAMS),--params $(PARAMS)) \
		$(if $(FEE),--fee $(FEE)) $(if $(INITIAL_CASH),--initial-cash $(INITIAL_CASH))

run-paper: ## Paper trade against testnet prices (the safe default)
	$(CLI) run --paper

clean: ## Remove build output
	cargo clean
