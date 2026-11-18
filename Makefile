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
# Optional: bars of history a strategy keeps (default 500; daily stock
# history rarely has that many before FROM).
HISTORY_LEN  ?=
# eToro stock examples (config/examples/etoro/params): daily bars.
ETORO_STOCKS ?= SPY.RTH,QQQ.RTH,AAPL.RTH,MSFT.RTH,AMZN.RTH

CLI := cargo run -p cli --
# Optional features `check` also covers, so code behind them can't rot.
FEATURES := --features strategy/rhai,backtest/rhai,cli/rhai

.DEFAULT_GOAL := help
.PHONY: help build test lint fmt fmt-check doc check \
        migrate-up migrate-down migrate-status migrate-fresh \
        fetch-data backtest etoro-stocks-data etoro-stocks-backtest console run-paper clean

help: ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*## "} {printf "  %-15s %s\n", $$1, $$2}'

## --- Develop -------------------------------------------------------------

build: ## Build every crate
	cargo build --workspace

test: ## Run all tests (optional features on)
	cargo test --workspace $(FEATURES)

lint: ## Clippy with warnings as errors, without and with optional features
	cargo clippy --workspace --all-targets -- -D warnings
	cargo clippy --workspace --all-targets $(FEATURES) -- -D warnings

fmt: ## Format code
	cargo fmt --all

fmt-check: ## Fail if code is not formatted
	cargo fmt --all --check

doc: ## Build docs, failing on broken links or doc warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps $(FEATURES)

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

fetch-data: ## Cache klines into data/ (VENUE=binance|binance-usdm|etoro|max)
	$(CLI) fetch-data --venue $(VENUE) --symbol $(SYMBOL) --interval $(INTERVAL) --from $(FROM)

backtest: ## Backtest STRATEGY on VENUE's SYMBOL over FROM..TO (SLIPPAGE_BPS required; PARAMS, FEE, INITIAL_CASH)
	@test -n "$(SLIPPAGE_BPS)" || { echo "set SLIPPAGE_BPS, e.g. make backtest SLIPPAGE_BPS=5"; exit 2; }
	$(CLI) backtest --venue $(VENUE) --strategy $(STRATEGY) --symbol $(SYMBOL) --interval $(INTERVAL) \
		--from $(FROM) --to $(TO) --slippage-bps $(SLIPPAGE_BPS) $(if $(PARAMS),--params $(PARAMS)) \
		$(if $(FEE),--fee $(FEE)) $(if $(INITIAL_CASH),--initial-cash $(INITIAL_CASH)) \
		$(if $(HISTORY_LEN),--history-len $(HISTORY_LEN))

etoro-stocks-data: ## Cache daily eToro stock history since 2022 (ETORO_PUBLIC_KEY / ETORO_PRIVATE_KEY)
	$(CLI) fetch-data --venue etoro --symbol $(ETORO_STOCKS) --interval 1d --from 2022-01-01

# Each example vs the SPY benchmark, same dates and costs: $10,000, 5 bps,
# fee 0.0002 = eToro's $1 per stock order on ~$5,000-$9,000 orders.
etoro-stocks-backtest: ## Backtest the eToro stock examples vs buy-and-hold SPY (FROM, TO; daily bars)
	@for ex in "buy_and_hold SPY.RTH buy_and_hold-spy" "sma_trend AAPL.RTH sma_trend-aapl" \
	           "top_k_momentum AAPL.RTH,MSFT.RTH,AMZN.RTH,QQQ.RTH top_k_momentum-stocks"; do \
		set -- $$ex; echo "== $$1 on $$2 ($(FROM) to $(TO))"; \
		$(CLI) backtest --venue etoro --strategy $$1 --symbol $$2 --interval 1d \
			--from $(FROM) --to $(TO) --slippage-bps 5 --fee 0.0002 --initial-cash 10000 \
			--history-len 100 --params config/examples/etoro/params/$$3.toml || exit 1; \
	done

console: ## Local web console: script editor + backtests vs buy-and-hold (http://127.0.0.1:9186/)
	cargo run -p cli --features rhai -- console

run-paper: ## Paper trade against testnet prices (the safe default)
	$(CLI) run --paper

clean: ## Remove build output
	cargo clean
