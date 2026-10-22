# trade-bot

An event-driven trading bot in Rust. It trades Binance spot today and is
built so that more venues and asset classes (US equities next, options and
perpetual futures later) can be added without rewriting it.

The core idea: **a backtest and a live run execute the same strategy code
through the same engine loop.** Only the provider differs, either cached
candles with a simulated broker or Binance's WebSocket feed with a real
broker. If the two ever behaved differently in a way a strategy could see,
that would be a bug.

> **Status: v1.** Backtesting and paper trading (Binance testnet or Demo
> Trading) are the supported paths. Real-money trading exists but is behind
> three separate locks, and it has not been exercised against mainnet. No
> strategy here has a demonstrated edge: backtest results come from a stated
> fill model and are not a promise.

## Contents

- [Safety model](#safety-model)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Strategies](#strategies)
- [Backtesting](#backtesting)
- [Paper and live trading](#paper-and-live-trading)
- [Commands](#commands)
- [Project layout](#project-layout)
- [Known limitations](#known-limitations)
- [Development](#development)

More depth: [doc/architecture.md](doc/architecture.md).

## Safety model

These rules are invariants in the code, not just conventions:

- **Money is exact.** Prices, quantities, balances and fees are
  `rust_decimal::Decimal` wrapped in unit types (`Price`, `Qty`, `Notional`),
  never floats. Config and data files take money as strings
  (`fee = "0.001"`); a bare number is an error, because it would pass through
  a float first.
- **No lookahead.** A strategy sees market data only up to the current,
  closed candle. An order decided on a candle fills no earlier than the next
  one. A test rewrites every candle after time *t* and requires that nothing
  up to *t* changes.
- **Secrets come from the environment only.** API keys are read from
  `BINANCE_API_KEY` and `BINANCE_API_SECRET`. No config field can hold them:
  `api_key = …` in a file is an unknown-field error.
- **Safe by default.** The default is paper trading on testnet. Real money
  needs all three of `mode = "live"` in config, `exchange.network = "live"`,
  and `run --live --i-understand` typed on the command line. There is
  deliberately no `make` target for live.

## Quick start

Requirements: Rust (the toolchain is pinned in `rust-toolchain.toml` and
installed by `rustup` automatically) and about 1 GB of disk for the build.

```sh
git clone <this repo> && cd trade-bot
make check                      # format, lint, docs, tests

# Cache public history (no API key needed) and backtest on it.
make fetch-data SYMBOL=BTCUSDT FROM=2024-01-01
make backtest STRATEGY=buy_and_hold SYMBOL=BTCUSDT \
     FROM=2024-01-01 TO=2024-06-01 SLIPPAGE_BPS=5 PARAMS=my-params.toml
```

`my-params.toml` holds the strategy's parameters, for example:

```toml
instrument = "binance:BTCUSDT"
quote_amount = "1000"
```

To paper trade, see [Paper and live trading](#paper-and-live-trading).

## Configuration

`run` reads `config/config.toml` (or `--config <file>`, or `BOT_CONFIG`),
then applies `BOT_*` environment variables on top, nested with `__`:

```sh
BOT_EXCHANGE__NETWORK=demo BOT_STRATEGY__NAME=sma_trend cargo run -p cli -- run --paper
```

Start from the documented example, which a test loads so it can't go stale:

```sh
cp config/config.example.toml config/config.toml
```

`config/config.toml` and `config/*.local.toml` are gitignored. Unknown keys
are errors, so a typo fails loudly instead of falling back to a default.

| Section | Field | Meaning |
|---|---|---|
| top level | `mode` | `"paper"` (default) or `"live"` |
| top level | `whitelist` | The only instruments the bot may trade, e.g. `["binance:BTCUSDT"]` |
| `[exchange]` | `network` | `"testnet"` (default), `"demo"` or `"live"` |
| `[exchange]` | `interval` | Candle width: `1m`, `5m`, `15m`, `1h`, `4h`, `1d` |
| `[exchange]` | `http_timeout_secs` | Longest one REST request may take (default 10) |
| `[exchange]` | `rest_url`, `ws_url` | Host overrides; a paper run refuses real-money hosts |
| `[strategy]` | `name`, `params` | A registered strategy and its own parameter table |
| `[engine]` | `history_len` | Candles of history kept per instrument (default 500) |
| `[db]` | `url` | Where a run keeps its books; default `data/<network>.db` |

## Strategies

Strategies implement one trait and register themselves; there is no central
list to edit. The config's `strategy.name` picks one, and `strategy.params`
is deserialized into that strategy's own typed struct.

| Name | What it does |
|---|---|
| `buy_and_hold` | Buys once with a fixed quote amount and holds. A baseline: a strategy that can't beat it after costs has no edge. |
| `ratio_hop` | Holds one coin of a set and rotates `A → USDT → B` only when the post-fee estimate of B's units beats the units of B last held, so unit counts only ratchet up. Includes a freefall guard against rotating into a collapsing coin. Maximizes units, not value. |
| `sma_trend` | One coin, long or flat. Holds while the close is above its simple moving average (with an optional dead zone), and sits in cash otherwise. v1 spot can't short, so cash is the defensive position. |

Each strategy's module docs and parameter fields document its parameters.
To add one, create a file under `crates/strategy/src/builtin/`, implement
`Strategy`, and register it with `inventory::submit!`. It then runs in
backtests and live with no other changes.

A strategy's only window to the world is its `Context`: positions, recent
candles, open orders, params, and a way to submit signals. It never touches
the broker, the network or the database.

## Backtesting

```sh
cargo run -p cli -- fetch-data --symbol BTCUSDT,ETHUSDT --interval 1h --from 2024-01-01
cargo run -p cli -- backtest --strategy ratio_hop --params p.toml \
    --symbol BTCUSDT,ETHUSDT --from 2024-01-01 --to 2024-06-01 --slippage-bps 5
```

`fetch-data` downloads closed candles and instrument filters from Binance's
public market-data host into `data/` (gitignored), and fetches only the
ranges it doesn't have yet. `--slippage-bps` is required and has no
default: a market order never fills at the price the decision saw.

The report gives final equity, realized P&L, lowest cash, maximum drawdown,
fill and closed-trade counts, win rate, Sharpe ratio and rejected signals,
all as exact decimals. The fill model is stated, not hidden; see
[doc/architecture.md](doc/architecture.md#backtest-fill-model). Two
approximations to keep in mind: instrument filters are today's, not
historical, and a whitelist chosen today carries survivorship bias.

## Paper and live trading

Paper trading uses fake balances on one of two Binance environments. Keys
belong to one environment; a key used on another is refused with `-2015`.

| `exchange.network` | Keys from | Notes |
|---|---|---|
| `testnet` | testnet.binance.vision | The default |
| `demo` | demo.binance.com (Demo Trading) | Prices track mainnet |
| `live` | binance.com | Real money; needs all three locks |

```sh
export BINANCE_API_KEY=… BINANCE_API_SECRET=…   # better: from a gitignored .env
make run-paper
```

What a run does:

1. Checks the key with a read-only signed request before trading.
2. **Reconciles** with Binance. It records fills that happened while the bot
   was down, cancels orders it left open, and refuses to start if an order
   it placed under another database is still open. Orders other people
   placed are left alone and only logged.
3. Trades on closed candles. Orders, fills, cancels and rejections are
   logged by id; money values stay out of info-level logs.
4. On Ctrl-C, SIGTERM or SIGHUP, stops and cancels its open orders. A
   second signal skips that cleanup, and the next start finishes it.

Orders and fills are kept in a per-network SQLite database (default
`data/<network>.db`). It is stamped with its network, so a demo database
can't be opened for a live run. Positions survive restarts.

For a long-running bot, run it under a supervisor (for example systemd with
`Restart=always`). A crash or `kill -9` can't be handled in-process, and
until the bot restarts and reconciles, its orders on Binance go unwatched.

Live trading:

```sh
# config: mode = "live", [exchange] network = "live"
cargo run -p cli -- run --live --i-understand
```

## Commands

`make` with no target lists them.

| Target | Does |
|---|---|
| `make check` | Format check, clippy with warnings as errors, docs, tests. Run before every commit. |
| `make fetch-data` | Cache klines (`SYMBOL`, `INTERVAL`, `FROM`) |
| `make backtest` | Backtest (`STRATEGY`, `SYMBOL`, `FROM`, `TO`, `PARAMS`, `SLIPPAGE_BPS` required) |
| `make run-paper` | Paper trade on the configured fake-money network |
| `make migrate-up` / `-down` / `-status` / `-fresh` | Database migrations (`DATABASE_URL`) |

`run` also applies pending migrations to its own database on start.

## Project layout

```
crates/
  core/       money types, ids, Instrument, Candle, Order, Fill, Position
  strategy/   Strategy trait, Context, registry, built-in strategies
  exchange/   Provider / MarketData / Broker traits, paper broker, Binance
  engine/     the event loop shared by backtest and live, risk checks
  backtest/   kline cache, BacktestProvider, report
  storage/    SeaORM repositories: trades, holdings, order journal
  cli/        the `cli` binary: fetch-data, backtest, run
migration/    SeaORM schema migrations
config/       config.example.toml (committed); real configs are gitignored
doc/          architecture notes
```

## Known limitations

- Binance spot only. US equities, options and perpetual futures are planned
  but not implemented.
- No WebSocket reconnect: a dropped market-data connection stops the run,
  and the exit cleanup still runs.
- Strategy state isn't persisted. `ratio_hop` resumes on a coin it holds,
  but its rotation history restarts. A restart between the two legs of a
  rotation buys afresh.
- No cash balance in the brokers: buys aren't refused for lack of quote
  (the backtest report shows overspending as negative `min_cash`).
- BNB fees are valued at the BNB price in the trade's minute, an
  approximation, because Binance doesn't report its conversion rate.
- MySQL is supported by the storage layer but untested.

## Development

Every change ships with its tests and docs, and passes `make check`.
Backtest P&L paths have regression tests with hand-worked expected values.
Commits are one logical change each, with Conventional Commit subjects.
`CLAUDE.md` holds the full project rules.

No license file has been added yet.
