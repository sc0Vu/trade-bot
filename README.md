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
- [Containers and Render](#containers-and-render)
- [Commands](#commands)
- [Project layout](#project-layout)
- [Known limitations](#known-limitations)
- [Development](#development)
- [License](#license)

More depth: [doc/architecture.md](doc/architecture.md). Project page:
<https://sc0vu.github.io/trade-bot/>.

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

Prebuilt binaries for Linux (x86_64, arm64; glibc 2.35+), macOS (Apple
silicon, Intel) and Windows (x86_64) are on the
[releases page](https://github.com/sc0Vu/trade-bot/releases). Each archive
holds `trade-bot`, which is the same program as `cargo run -p cli --`
below, plus `config/config.example.toml` and the examples. Check it
against `SHA256SUMS`. Then `trade-bot run --paper` replaces
`make run-paper`.

To build from source you need Rust (the toolchain is pinned in
`rust-toolchain.toml` and `rustup` installs it automatically) and about
1 GB of disk for the build.

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
coin = "binance:BTCUSDT"
quote_amount = "1000"
fee = "0.001"        # match the backtest's fee
slippage_bps = "5"   # match SLIPPAGE_BPS
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

[`config/examples/`](config/examples/README.md) has a complete config for
each built-in strategy (paper on testnet, textbook params), runnable as
`run --paper --config config/examples/<strategy>.toml`.

`config/config.toml` and `config/*.local.toml` are gitignored. Unknown keys
are errors, so a typo fails loudly instead of falling back to a default.

| Section | Field | Meaning |
|---|---|---|
| top level | `mode` | `"paper"` (default) or `"live"` |
| top level | `whitelist` | The only instruments the bot may trade, e.g. `["binance:BTCUSDT"]` |
| `[exchange]` | `network` | `"testnet"` (default), `"demo"` or `"live"` |
| `[exchange]` | `interval` | Candle width: `1s`, `1m`, `5m`, `15m`, `1h`, `4h`, `1d` |
| `[exchange]` | `http_timeout_secs` | Longest one REST request may take (default 10) |
| `[exchange]` | `rest_url`, `ws_url` | Host overrides; a paper run refuses real-money hosts |
| `[strategy]` | `name`, `params` | A registered strategy and its own parameter table |
| `[engine]` | `history_len` | Candles of history kept per instrument (default 500) |
| `[db]` | `url` | Where a run keeps its books; default `data/<network>.db` |
| `[metrics]` | `enabled`, `listen`, `include_money`, `allow_public` | Prometheus `/metrics` during `run`; off, loopback-only and money-free by default |
| `[api]` | `enabled`, `listen`, `allow_public` | Read-only JSON API during `run` (default `127.0.0.1:9185`); a public address needs a token in `TRADEBOT_API_TOKEN` |
| `[log]` | `level` | What `run` logs, as a `RUST_LOG`-style filter (default `info,sqlx=warn`); `RUST_LOG`, when set, overrides it |
| `[log]` | `format` | Console lines as `text` (default) or `json` |
| `[log]` | `file` | Also append JSON lines here for a log shipper (Loki via `deploy/`, Datadog, Vector); unset = no file |

## Strategies

Strategies implement one trait and register themselves; there is no central
list to edit. The config's `strategy.name` picks one, and `strategy.params`
is deserialized into that strategy's own typed struct.

| Name | What it does |
|---|---|
| `avwap` | One coin, long or flat. Holds while the close is above its anchored VWAP (the exact average price of all trading since the start of the day, week or month, or since a recent swing low or high), cash below. |
| `buy_and_hold` | The benchmark. Buys one coin once with a fixed quote amount on its first candle, then holds forever. A strategy that can't beat it on the same coin after costs has no edge. |
| `dca` | Dollar-cost averaging. Buys a fixed quote amount of one coin once per UTC hour, day, week or month (optionally at most `max_buys` times) and holds. No prediction: spreading the entry removes the risk of buying everything at a top, not the coin's direction; it keeps buying all the way down a decline. Each buy pays a fee and slippage. |
| `ratio_hop` | Holds one coin of a set and rotates `A → USDT → B` only when the post-fee estimate of B's units beats the units of B last held, so unit counts only ratchet up. Includes a freefall guard against rotating into a collapsing coin. Maximizes units, not value. |
| `donchian_breakout` | One coin. Buys when the close breaks above the highest high of the previous `channel_period` candles, sells when it breaks below their lowest low. Bets breakouts continue; whipsaws in ranges. |
| `dual_ma_cross` | One coin, long or flat. Holds while the fast moving average is above the slow one, cash otherwise. A quicker variant of `sma_trend`; whipsaws in ranges. |
| `grid` | One coin, spot grid. Splits a price range into levels, buys a fixed quote amount at each level and sells that lot one level up, so each round trip earns one step minus two fees (a grid whose step doesn't cover them is refused). Bets on prices oscillating in the range; falling, it ends holding every level below cost (optional `stop_price` sells all and stops), rising, it sells out. |
| `momentum_rotation` | Several coins. Holds the coin with the best return over the last `lookback` bars while that return beats `min_momentum`, and goes fully to cash when none does. Bets that recent winners keep winning; fails on sharp reversals. |
| `rsi_reversion` | One coin. Buys when RSI falls below `oversold`, sells when it rises above `overbought`. Bets on ranging markets; in a sustained downtrend it catches falling knives and holds them down. Included partly as the counterexample to the trend filters. |
| `sma_trend` | One coin, long or flat. Holds while the close is above its simple moving average (with an optional dead zone), and sits in cash otherwise. v1 spot can't short, so cash is the defensive position. |

Each strategy's module docs and parameter fields document its parameters.
To add one, create a file under `crates/strategy/src/builtin/`, implement
`Strategy`, and register it with `inventory::submit!`. It then runs in
backtests and live with no other changes. Add an example config under
`config/examples/` too: a test requires one per registered strategy.

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

`--venue max` does the same against MAX (MaiCoin): `max:BTCUSDT`,
`max:BTCTWD` and the other MAX markets, from 1m bars up (MAX has no 1s
klines). Pass your MAX fee rate with `--fee` (the `0.0015` below is a
placeholder, not a verified MAX rate), and expect more slippage
than on Binance: MAX's USDT books are thin. MAX's REST klines carry no
quote volume, so it is estimated from the bar's prices.

```sh
cargo run -p cli -- fetch-data --venue max --symbol BTCTWD --interval 1h --from 2024-01-01
cargo run -p cli -- backtest --venue max --strategy sma_trend --params p.toml \
    --symbol BTCTWD --from 2024-01-01 --to 2026-10-01 --fee 0.0015 --slippage-bps 20
```

The report gives final equity, realized P&L, lowest cash, maximum drawdown,
fill and closed-trade counts, win rate, Sharpe ratio and rejected signals,
all as exact decimals. The fill model is stated, not hidden; see
[doc/architecture.md](doc/architecture.md#backtest-fill-model). Two
approximations to keep in mind: instrument filters are today's, not
historical (on old, low prices today's coarser tick makes taker fills
slightly worse: SOL at about 1.84 in 2021 with a 0.01 tick costs up to
about 0.5% per fill), and a whitelist chosen today carries survivorship
bias.

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

For your own UI or scripts, enable `[api]`: `GET /status`, `/positions`,
`/open-orders`, `/orders?limit=N` and `/trades?limit=N` return JSON (money as
decimal strings). A trade's `client_order_id` matches its order in
`/orders`; its `order_id` is per run (`o-1`, `o-2`, … every run). It is read-only by design: nothing over HTTP can place or
cancel an order or switch networks.

To watch it, enable `[metrics]` (and `[log] file` for logs) and start the
Prometheus, Alertmanager, Loki and Grafana stack in
[`deploy/`](deploy/README.md): a dashboard with the bot's logs, plus alerts
for a stalled feed, a stuck strategy, Binance errors and the bot being
down. Datadog, Grafana Cloud and other backends read the same `/metrics`
and JSON log lines.

For a long-running bot, run it under a supervisor (for example systemd with
`Restart=always`). A crash or `kill -9` can't be handled in-process, and
until the bot restarts and reconciles, its orders on Binance go unwatched.

Live trading:

```sh
# config: mode = "live", [exchange] network = "live"
cargo run -p cli -- run --live --i-understand
```

### MAX (MaiCoin)

MAX has no testnet, so the bot never sends it orders yet. With
`exchange.venue = "max"`, `run --paper` trades MAX's live closed bars
against the local paper broker: the same fill model as `backtest` (next
bar's open, stated fee and slippage), no API key, nothing sent to MAX.
Positions are in memory: each run starts flat from `paper.initial_cash`, and
the read-only API is off.

```toml
whitelist = ["max:BTCTWD"]
[exchange]
venue = "max"
interval = "1m"
[paper]
initial_cash = "30000"   # TWD, the first instrument's quote
fee = "0.0015"           # your MAX fee rate (placeholder)
slippage_bps = "20"
```

`mode = "live"` or `--live` with MAX is refused.

## Containers and Render

`Dockerfile` builds the bot (`trade-bot run --paper` by default);
`render.yaml` deploys it to Render as a background worker. Test in this
order, on testnet keys, before anything touches real money.

**1. Locally, with Docker.** Put testnet keys in `.env`
(`BINANCE_API_KEY=…`, `BINANCE_API_SECRET=…`) and a paper config in
`config/config.toml`, then:

```sh
docker build -t trade-bot .
docker run -d --name tb --env-file .env -e BOT_LOG__FORMAT=json \
  -v "$PWD/config/config.toml:/app/config/config.toml:ro" \
  -v tb-data:/app/data trade-bot
docker logs -f tb
```

| Check | How | Expect |
|---|---|---|
| Starts | `docker logs tb` | `reconciled with Binance`, then `starting` with the run tag |
| Trades | Wait for the strategy to act | `order submitted` / `fill` lines with `tb…-o-N` ids |
| Graceful stop | `docker stop -t 120 tb` | Close-out log, open orders cancelled on testnet |
| Books survive | `docker start tb` | `resuming saved strategy state`, the same positions |
| Crash | `docker kill tb`, then `docker start tb` | Startup reconcile picks up orders and fills from the downtime |

A fresh volume with orders still open on the account makes the bot refuse
to start (an unknown `tb…` order). That's the guard working: reuse the
volume.

**2. On Render, still testnet.** *New > Blueprint* with this repo reads
`render.yaml`. Then, in the dashboard:
- Set `BINANCE_API_KEY` and `BINANCE_API_SECRET` (testnet keys).
- Upload your config as a *Secret File* named `config.toml`; `BOT_CONFIG`
  points at `/etc/secrets/config.toml`.
- Repeat the checks: **Logs** for startup, then **Manual Deploy** for the
  graceful stop and restart. After a redeploy you should see
  `resuming saved strategy state`: proof the disk kept the books.

What the blueprint settles, and why:
- **Background worker, not a web service.** The bot needs no inbound port,
  and free web services sleep when idle, which would stop trading.
- **Frankfurt region.** Binance refuses requests from US IP addresses
  (HTTP 451), and Render's other regions are in the US. Check that
  Binance serves your own jurisdiction too.
- **Persistent disk at `/app/data`.** The books must survive deploys. A
  disk also means Render stops the old instance before starting the new
  one, so two bots never trade one account.
- **`maxShutdownDelaySeconds: 120`.** Time for the close-out after
  SIGTERM; Render's default is 30 s.
- Render restarts a crashed container, so it acts as the supervisor.

Monitoring: Render keeps stdout (`BOT_LOG__FORMAT=json` for log streams to
Datadog or other services). A worker has no public address, so Prometheus
can't scrape it from outside Render. Run the collector as another service
in the same region, or ship metrics with Grafana Alloy or the Datadog
agent.

**3. Live.** Only after the testnet run behaves: live keys, a config with
`mode = "live"` and `[exchange] network = "live"`, a new disk (one database
per network), and the start command set to `run --live --i-understand` in
Render's settings. It's never in the image (invariant 7).

## Commands

`make` with no target lists them.

| Target | Does |
|---|---|
| `make check` | Format check, clippy with warnings as errors, docs, tests. Run before every commit. |
| `make fetch-data` | Cache klines (`VENUE`, `SYMBOL`, `INTERVAL`, `FROM`) |
| `make backtest` | Backtest (`VENUE`, `STRATEGY`, `SYMBOL`, `FROM`, `TO`, `PARAMS`, `FEE`, `INITIAL_CASH`; `SLIPPAGE_BPS` required) |
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
  api/        read-only HTTP API: status, positions, orders, trades
  cli/        the `cli` binary: fetch-data, backtest, run
migration/    SeaORM schema migrations
config/       config.example.toml and examples/ (committed); real configs are gitignored
doc/          architecture notes
deploy/       Prometheus, Alertmanager, Loki + Alloy, Grafana (compose) and alert rules
```

## Known limitations

- Binance spot only. US equities, options and perpetual futures are planned
  but not implemented.
- A market-data outage longer than `exchange.max_outage_secs` (default 10
  min) stops the run. Bars missed during a shorter one are backfilled as
  history; only the newest is traded on.
- Strategy state is saved per strategy and restored only under the same
  params; changing a strategy's params (say, its coin list) starts it fresh.
- The backtest broker trades from `--initial-cash` in one quote asset and
  refuses buys it can't cover, as Binance would. A buy placed right after
  a market sell may count that sale's expected proceeds (last close); if
  the sale raises less, the buy is dropped unfilled at the next bar.
- BNB fees are valued at the BNB price in the trade's minute, an
  approximation, because Binance doesn't report its conversion rate.
- MySQL is supported by the storage layer but untested.
- Rate limits use Binance's published values with 20% headroom; another
  program on the same IP is only noticed through Binance's usage header.

## Development

Every change ships with its tests and docs, and passes `make check`.
Backtest P&L paths have regression tests with hand-worked expected values.
Commits are one logical change each, with Conventional Commit subjects.
`CLAUDE.md` holds the full project rules.

CI (`.github/workflows/ci.yml`) runs `make check`, validates the
`deploy/` configs with each tool's own image, and builds the container
image and starts it once, on every push and pull request. Pushing a tag `vX.Y.Z` that matches the workspace version runs
`release.yml`: it builds every platform (Linux with OpenSSL linked
statically through the `vendored-tls` feature), smoke-tests each binary,
and publishes the archives with `SHA256SUMS` as a GitHub release. *Run
workflow* on the Release action builds the archives without publishing.
`pages.yml` publishes `site/` (a single static page) to
<https://sc0vu.github.io/trade-bot/> on every push to `main` that touches it.

## License

MIT. See [LICENSE](LICENSE).
