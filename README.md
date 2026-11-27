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

[![Deploy to Render](https://render.com/images/deploy-to-render-button.svg)](https://render.com/deploy?repo=https://github.com/sc0Vu/trade-bot)
[![Deploy on any VPS](https://img.shields.io/badge/Deploy-any%20VPS-0080FF)](deploy/vps/README.md)

Both deploy paper trading on testnet. You add the API keys yourself:
Render asks for them on its deploy form (stored by Render, never in the
repository). On a VPS you write them to a file on the server after logging
in. Render: a paid worker plan with a disk (Frankfurt), and the bot starts
only once you upload your config as a Secret File (see
[Containers and Render](#containers-and-render)).

## Contents

- [Safety model](#safety-model)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Strategies](#strategies)
- [Backtesting](#backtesting)
- [Console (web)](#console-web)
- [Paper and live trading](#paper-and-live-trading)
- [Running as a service](#running-as-a-service)
- [Containers and Render](#containers-and-render)
- [Commands](#commands)
- [Project layout](#project-layout)
- [Known limitations](#known-limitations)
- [Development](#development)
- [License](#license)

More depth: [doc/architecture.md](doc/architecture.md); writing strategies:
[doc/scripting.md](doc/scripting.md) (scripts),
[doc/builtin-strategies.md](doc/builtin-strategies.md) (Rust). Project page:
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
  `BINANCE_API_KEY` and `BINANCE_API_SECRET` (Binance spot and futures) or
  `ETORO_PUBLIC_KEY` and `ETORO_PRIVATE_KEY` (eToro). No config field can
  hold them: `api_key = …` in a file is an unknown-field error.
- **Safe by default.** The default is paper trading on testnet. Real money
  needs all three of `mode = "live"` in config, `exchange.network = "live"`,
  and `run --live --i-understand` typed on the command line. There is
  deliberately no `make` target for live.
- **A drawdown limit the strategy can't override.** With `[engine]
  kill_drawdown` set, the engine closes every position and stops the run
  once equity falls that far from its peak, and refuses to start again
  until someone runs it with `--clear-kill-switch`.

## Quick start

On Linux or macOS, install the latest release (checksum verified, into
`~/.local/bin`; the example configs and scripts go to
`~/.local/share/trade-bot`):

```sh
curl -fsSL https://sc0vu.github.io/trade-bot/install.sh | sh
```

The script is [`site/install.sh`](site/install.sh); `VERSION=0.3.0` pins a
release, `INSTALL_DIR` / `SHARE_DIR` move it. Or by hand: prebuilt
binaries for Linux (x86_64, arm64; glibc 2.35+), macOS (Apple
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
`run --paper --config config/examples/<strategy>.toml`. The binary carries
them too, so this works without the repository:

```sh
trade-bot config list                          # strategies, one line each
trade-bot config init                          # asks: strategy, venue, coins, interval, costs, params
trade-bot config init --strategy sma_trend     # writes config/config.toml from its example as is
trade-bot config check                         # what `run` would refuse, without connecting
trade-bot config check --config my.toml --live # as `run --live` (config's two locks)
trade-bot config check --env-file ~/.trade-bot.env  # keys from the service's file, not the shell
```

`config init` without `--strategy` (or with `-i`) asks for each setting.
Each default comes from the strategy's example, and each param's help is the
example's comment beside it. Changing the coins renames them in the params.
On eToro it asks the market: US stocks and ETFs (regular hours, which
adds `.RTH`, or extended; daily bars by default) or crypto (`BTC`, not
`BTCUSDT`). Money answers must be exact decimals, and a bare `0.5` in a table is
refused. Nothing is written until the config loads and the strategy builds;
otherwise the questions repeat with your answers as defaults. It writes
paper configs only (testnet or demo, or local paper fills on MAX and
USDⓈ-M) and never asks for keys: going live stays a deliberate edit of
`mode` and `exchange.network`.

`config check` loads the config as `run` does (file, then `BOT_*`), and
reports every refusal at once: the live gate, a URL override pointing a
fake-money run at mainnet, keys missing from the environment (names only;
values are never printed), the API token, the strategy's params, a
tripped kill switch. It also shows where the run keeps its books. It
connects to nothing, so a bad key, an unlisted symbol or the balance only
show at `run`. Exit status 1 when `run` would refuse. With `--env-file`
the keys are read from that file, the one `service install --env-file`
gets: refused unless chmod 600, `KEY=value` lines only (no `export`, no
`$`, backquotes, backslashes or inner quotes, which systemd and launchd
would read differently). `--i-understand` is
never accepted there: it is typed at each live start.

`config/config.toml` and `config/*.local.toml` are gitignored. Unknown keys
are errors, so a typo fails loudly instead of falling back to a default.

| Section | Field | Meaning |
|---|---|---|
| top level | `mode` | `"paper"` (default) or `"live"` |
| top level | `name` | Optional bot name (`a-z 0-9 _ -`, up to 64). Several bots on one machine each get their own default database (`data/<network>[-usdm\|-etoro]-<name>.db`) and kill-switch marker; also give each its own `[api] listen` port if the API is on |
| top level | `whitelist` | The only instruments the bot may trade, e.g. `["binance:BTCUSDT"]`, or bare symbols on `exchange.venue` (`["BTCUSDT"]`). In strategy params, a string naming a whitelisted symbol (`coin = "BTCUSDT"`, `coins = [...]`, map keys) gets its venue too; a symbol on two venues (a spot + perpetual hedge) must be written in full |
| `[exchange]` | `venue` | `"binance"` (default, spot), `"binance-usdm"` (perpetuals), `"etoro"` or `"max"`; the whitelist must use the same venue |
| `[exchange]` | `network` | `"testnet"` (default), `"demo"` or `"live"` |
| `[exchange]` | `interval` | Candle width: `1s`, `1m`, `5m`, `15m`, `1h`, `4h`, `1d` |
| `[exchange]` | `http_timeout_secs` | Longest one REST request may take (default 10) |
| `[exchange]` | `rest_url`, `ws_url` | Host overrides; a paper run refuses real-money hosts |
| `[exchange]` | `max_outage_secs` | Longest market-data outage before the run stops (default 600) |
| `[exchange]` | `user_stream` | Binance spot: learn of fills over the user-data WebSocket, REST still authoritative (default `true`) |
| `[paper]` | `initial_cash`, `fee`, `slippage_bps` | Local paper broker for MAX and for `binance-usdm` with `fills = "local"` |
| `[perp]` | `fills`, `leverage`, `margin`, `brackets_from`, `clearance` | Perpetuals (`binance-usdm`): local paper fills or venue orders, leverage, isolated or cross margin |
| `[etoro]` | `w8ben_signed` | Set `true` once your W-8BEN is on file; a real account refuses US stocks without it |
| `[earn]` | `enabled`, `ratio`, `band`, `product`, `locked_days`, `interval_minutes` | Binance live only: keep a share of idle quote cash in Simple Earn |
| `[strategy]` | `name`, `params` | A registered strategy and its own parameter table |
| `[engine]` | `history_len` | Candles of history kept per instrument (default 500) |
| `[engine]` | `delist_exit_hours` | Hours before a scheduled delisting when the engine closes the position (default 24) |
| `[engine]` | `kill_drawdown`, `kill_capital` | Kill switch, off unless both are set: once equity (`kill_capital` plus the run's P&L) falls `kill_drawdown` below its peak, the engine closes everything and stops; the next start is refused until `run --clear-kill-switch`. Backtests: `--kill-drawdown` |
| `[db]` | `url` | Where a run keeps its books; default `data/<network>.db` |
| `[metrics]` | `enabled`, `listen`, `include_money`, `allow_public` | Prometheus `/metrics` during `run`; off, loopback-only and money-free by default |
| `[api]` | `enabled`, `listen`, `allow_public` | Read-only JSON API during `run` (default `127.0.0.1:9185`); a public address needs a token in `TRADEBOT_API_TOKEN` |
| `[api]` | `dashboard` | A read-only web dashboard at `http://<listen>/` while the bot runs (needs `enabled`): positions with value and P&L, open orders, latest bars, recent trades and orders, updated as each event is handled (server-sent events, `/events`) |
| `[log]` | `level` | What `run` logs, as a `RUST_LOG`-style filter (default `info,sqlx=warn`); `RUST_LOG`, when set, overrides it |
| `[log]` | `format` | Console lines as `text` (default) or `json` |
| `[log]` | `file` | Also append JSON lines here for a log shipper (Loki via `deploy/`, Datadog, Vector); unset = no file |

## Strategies

Strategies implement one trait and register themselves; there is no central
list to edit. The config's `strategy.name` picks one, and `strategy.params`
is deserialized into that strategy's own typed struct.

| Name | What it does |
|---|---|
| `atr_breakout` | One coin, long or flat. Buys when the close is more than `entry_mult` average true ranges (ATR) above its `period`-bar average, sells when it falls `exit_mult` ATRs below the highest high of those bars. ATR sizes both thresholds to recent volatility, so noise alone crosses neither. Bets breakouts become trends; each false breakout loses about (`entry_mult` + `exit_mult`) ATRs plus costs. |
| `avwap` | One coin, long or flat. Holds while the close is above its anchored VWAP (the exact average price of all trading since the start of the day, week or month, or since a recent swing low or high), cash below. |
| `bollinger_reversion` | One coin. Buys when the close falls below the lower Bollinger band (`k` standard deviations under the `period` mean), sells once it is back above the mean. Like `rsi_reversion` it bets on ranges and holds a long way down in a downtrend (no stop); the band widens with volatility. |
| `buy_and_hold` | The benchmark. Buys one coin once with a fixed quote amount on its first candle, then holds forever. A strategy that can't beat it on the same coin after costs has no edge. |
| `dca` | Dollar-cost averaging. Buys a fixed quote amount of one coin once per UTC hour, day, week or month (optionally at most `max_buys` times) and holds. No prediction: spreading the entry removes the risk of buying everything at a top, not the coin's direction; it keeps buying all the way down a decline. Each buy pays a fee and slippage. |
| `rebalance` | Holds fixed weights of a stated capital (for example half BTC, half USDT, or several coins) and trades every coin back to its target once any weight drifts more than `band`. Sells some of what rose, buys some of what fell, so it earns from swings that come back without predicting direction; trails holding in a one-way trend, and pays fee and slippage on each rebalance. |
| `risk_parity` | Several coins, each weighted by the inverse of its recent volatility (`exposure` of a stated capital; the rest cash), traded back when a weight drifts past `band`. Each coin carries about the same share of the swings, so the portfolio isn't just a bet on its wildest coin. Predicts no returns; ignores correlation (high between coins); holds less of a volatile coin in its rallies and trails holding the winner. |
| `ratio_hop` | Holds one coin of a set and rotates `A → USDT → B` only when the post-fee estimate of B's units beats the units of B last held, so unit counts only ratchet up. Includes a freefall guard against rotating into a collapsing coin. Maximizes units, not value. |
| `donchian_breakout` | One coin. Buys when the close breaks above the highest high of the previous `channel_period` candles, sells when it breaks below their lowest low. Bets breakouts continue; whipsaws in ranges. |
| `funding_carry` | Cash-and-carry on perpetual funding: long Binance spot, short the same quantity of its USDⓈ-M perpetual while the week's funding annualizes above `enter_apr`; closed below `exit_apr`, or when the price rises `max_rise` over the short's entry (before liquidation). Market-neutral; earns funding, pays four fees a round trip. **Backtest only** (`backtest --perp-symbol`): a run trades one venue. BTC 4h, 2022 to Nov 2026, 1,000 spot + 500 futures at 2x: 1,500 → 1,601 (max drawdown 1.1%). The funding itself paid 7.2% a year of notional, but every rally exit leaves the short's loss in the futures wallet and the spot gain in the spot wallet; with no transfer between them the hedge shrinks, so it captured far less. |
| `dual_ma_cross` | One coin, long or flat. Holds while the fast moving average is above the slow one, cash otherwise. A quicker variant of `sma_trend`; whipsaws in ranges. |
| `grid` | One coin, spot grid. Splits a price range into levels, buys a fixed quote amount at each level and sells that lot one level up, so each round trip earns one step minus two fees (a grid whose step doesn't cover them is refused). Bets on prices oscillating in the range; falling, it ends holding every level below cost (optional `stop_price` sells all and stops), rising, it sells out. |
| `macd_cross` | One coin, long or flat. Holds while the MACD line (fast EMA minus slow EMA) is above its signal line, cash otherwise. Turns earlier than `dual_ma_cross`, so it whipsaws more in ranges and still lags turns. |
| `momentum_rotation` | Several coins. Holds the coin with the best return over the last `lookback` bars while that return beats `min_momentum`, and goes fully to cash when none does. Bets that recent winners keep winning; fails on sharp reversals. |
| `long_short_momentum` | Several coins that can be shorted (USDⓈ-M perpetuals). Ranks them by return over `lookback` bars, goes long the best `k` and short the worst `k`, each at `capital` / 2k, so the market's direction mostly cancels and it earns the spread between winners and losers. `rebalance_bars` ranks only every n bars (epoch-aligned). Flips close reduce-only first. Measured on 10 perps, 4h, 2022–2026: deciding every bar it made +157% before costs and −5% after (8,961 fills); walk-forward with daily or weekly rebalancing, the picks chained +88% out of sample against +26% for the median parameters. The coins are today's survivors, which flatters it; edge unverified live. |
| `top_k_momentum` | Several coins. Ranks them by return over `lookback` bars and holds the best `k` that beat `min_momentum`, each at 1/k of a stated capital (an empty slot stays cash), rotating as the ranking changes. `momentum_rotation` with k slots: less riding on one coin's reversal, more trades. Optional overlays: a trend filter (`trend_period`: held only above its SMA, else that slot is cash), several lookbacks averaged (`lookbacks`), inverse-volatility weights (`weighting = "inverse_vol"`), and a rebalance schedule (`rebalance_every`) with a trailing stop between rebalances (`trail_pct`). Fails on sharp reversals and churning leadership; coins picked with hindsight flatter any backtest. |
| `pair_reversion` | Two coins. Holds whichever is cheap relative to the other: when their price ratio sits more than `entry_z` standard deviations from its `period` mean, switches all of capital into the cheap one (sell, then buy with the proceeds), else keeps what it holds. Bets the ratio mean-reverts; loses when it trends (ETH/BTC through 2022–2025). Spot can't short, so it is always exposed to the pair's common move; each switch is a round trip of costs. |
| `rsi_reversion` | One coin. Buys when RSI falls below `oversold`, sells when it rises above `overbought`. Bets on ranging markets; in a sustained downtrend it catches falling knives and holds them down. Optional `trend_period` buys dips only while the close is above its moving average (pullbacks in an uptrend); it filters entries, not exits. Included partly as the counterexample to the trend filters. |
| `stochastic` | One coin. Buys when the stochastic oscillator's %K crosses above %D while below `oversold`, sells when it crosses below while above `overbought`. A turn from the extreme of the recent range: mean reversion like `rsi_reversion`, with the same falling-knife risk (no stop); trades rarely. |
| `sma_long_short` | One coin, `sma_trend` both ways: long above the moving average, short below it on a USDⓈ-M perpetual (backtest), flat below it on spot. Fixed notional per entry; a flip closes first (reduce-only) and opens the other side at the next decision, so a refused order never leaves a losing position open. Pays whipsaws and funding on both sides. |
| `sma_trend` | One coin, long or flat. Holds while the close is above its simple moving average (with an optional dead zone), and sits in cash otherwise. v1 spot can't short, so cash is the defensive position. |
| `ichimoku` | One coin. Buys when the close is above the Ichimoku cloud and Tenkan is above Kijun; sells when the close drops below the cloud. The cloud uses spans computed `displacement` bars back, as charts plot them: only past data. Few trades, late in and late out; a wide cloud gives back much of a move. |
| `supertrend` | One coin, long or flat on the SuperTrend: bands `mult` ATRs around the bar's midpoint that only ratchet toward price, flipping when a close breaks through. Long while it says up. A trend rule with an ATR trailing stop: fewer whipsaws than an average cross, but gives back up to `mult` ATRs at every turn. |
| `trailing_stop` | One coin. Buys on the first candle, sells once the close falls `trail_pct` below the highest close since entry, buys back once it rises `reentry_pct` above the lowest close since the exit. The simplest drawdown cap; in chop it sells near lows and buys back higher (a round trip each time), and a fast crash fills at the next open, past the stop. |
| `tsmom_long_short` | Time-series momentum both ways, as the published research tested it. Each coin: long while its return over `lookback` bars is positive, short while negative (flat on spot), sized for `target_vol`. A flip closes first (reduce-only) and opens the other side at the next decision, so a refused order never leaves a losing position stuck. Binance BTC perpetual, 4h, 2022 to July 2026, 2x: 1,000 → 2,136 (drawdown 38%) against buy-and-hold's 997 (65%); it earned in 2022 (1,097 against 369) and trailed in the 2023–2026 rise (2,038 against 2,852). One coin, one path, edge unverified. |
| `turn_of_month` | One instrument, held over the turn of each month (bars closing in the month's last `enter_days` days, through the new month's `hold_bars`-th bar), cash otherwise. A documented equity calendar effect, not a forecast; few trades. On eToro SPY 2020–2026 it made +28% against buy-and-hold's +133%, with a 9.7% drawdown against 33%: in the market about a quarter of the time, it earned about that share. |
| `tsmom` | Time-series momentum (Moskowitz, Ooi and Pedersen), long-only. Each coin on its own: held while its return over `lookback` bars is positive, at `target_vol` divided by its annualized volatility (capped, each coin at most 1/N of capital), out otherwise. Bets trends persist and cuts size when swings widen; loses in chop (each false signal is a round trip), lags turns by up to `lookback` bars, and trades as volatility moves the weight. |
| `vol_target` | One coin, held at a weight of a stated capital that aims at a target volatility: `target_vol` divided by the coin's recent annualized volatility, capped at `max_weight`. Holds more when calm, less in wide swings, which cuts most of the drawdown that comes with them; predicts no returns, trades as volatility moves the weight, and buys back late after a crash. |
| `volatility_breakout` | Larry Williams' volatility breakout, one coin. After each closed bar a buy stop sits `k` times that bar's high-low range above its close; if the next bar trades through it the coin is bought there and sold at that bar's close. The first built-in to use stop orders. Bets a move bigger than a fraction of the last range keeps going within the bar; pays two taker fees and two slippages per one-bar trade. |

Each strategy's module docs and parameter fields document its parameters.

**Adding your own strategy.** Two ways:

- **A script** (no Rust, no rebuild): write a Rhai file and point a config
  at it, below. The console edits, checks and backtests scripts.
  Guide: [doc/scripting.md](doc/scripting.md).
- **A built-in** (Rust, compiled in): a file under
  `crates/strategy/src/builtin/` that implements `Strategy` and registers
  itself with `inventory::submit!`, plus an example config, tests and docs.
  Guide: [doc/builtin-strategies.md](doc/builtin-strategies.md).

Either way it runs in backtests, paper and live with no other changes.

A strategy's only window to the world is its `Context`: positions, recent
candles, open orders, params, and a way to submit signals. It never touches
the broker, the network or the database.

### Script strategies (Rhai)

A strategy can also be a [Rhai](https://rhai.rs) script, loaded at startup,
with no Rust and no recompiling. Release binaries and the container include
it; from source, build with `--features rhai`.

The full script API, a worked example and a checklist for vetting a
script are in [doc/scripting.md](doc/scripting.md).

```toml
[strategy]
name = "rhai"
[strategy.params]
script = "strategies/sma_trend.rhai"   # relative to the working directory
coin = "binance:BTCUSDT"               # the rest are the script's own params
sma_period = 50
quote_amount = "1000"
fee = "0.001"
slippage_bps = "5"
```

```rhai
fn on_candle(ctx, c) {
    let hist = ctx.history(c.instrument, 2);   // only the newest 2 bars
    if hist.len() < 2 { return; }
    let rising = c.close > hist[0].close;
    if rising && ctx.position(c.instrument).is_flat {
        return market_buy_quote(c.instrument, 100.0);
    }
}
```

The script defines whichever hooks it needs (`on_start`, `on_candle`,
`on_fill`, `on_order`, `on_rejected`, `check_params`, `restore`) and returns
signals from them. The same rules hold as for compiled strategies:

- **Exact money.** Scripts have no floats: `0.001` is an exact decimal.
- **Sandboxed.** No files, network, clock or randomness, and a budget of
  1,000,000 operations per call.
- **A script error stops the run.** The bot never keeps trading on a
  broken strategy.
- **State survives restarts.** It is kept in `this` and is only restored
  into the same script.

`strategies/sma_trend.rhai` is a port of `sma_trend`. A test requires
identical fills to the built-in, and a 2021–2026 daily BTC backtest gives the
same report. Scripts are slower: in a release build, six months of 1-minute
bars take about 12 s against 3 s for the built-in (about 35 µs a bar). Ask
for the bars you need with `ctx.history(id, n)` rather than the whole
history. That is fine for candle intervals, but don't expect native speed.
The full API is documented in `crates/strategy/src/script/rhai.rs`.

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
Like a live run, a backtest first replays the last `--history-len` bars
(default 500) before `--from` as warm-up, so its first decision sees a full
history; fetch from earlier than `--from` to have them (it warns if they
aren't cached).

Delistings are replayed without hindsight. Announced ones come from
`config/delistings/binance.toml` (announcement time, delisting time,
source) and reach the strategy when they were announced; the engine sells
before the delisting as it would live. A series that just stops (a halt
nobody announced, like LUNA's or FTT's in 2022), or a gap longer than
`--max-gap-hours` (48), is cut there with no notice: what was held stays
at its last price, as it did for real holders. A symbol the venue later
reused for another coin is never spliced onto the first.

### Tuning without fooling yourself: `sweep`

Pick a strategy's parameters by backtesting them on the same period and
the best result measures the fit to that past, not the strategy. `sweep`
tests a parameter grid walk-forward: on each train window it backtests
every combination and picks the best; it then scores only that pick on the
window right after, which the pick never saw. Windows advance by the test
length. The grid is the params file plus a `[sweep]` table:

```toml
coin = "BTCUSDT"
quote_amount = "900"
fee = "0.001"
slippage_bps = "5"
[sweep]                      # every combination: 4 × 3 = 12
sma_period = [20, 50, 100, 200]
band_pct = ["0", "0.01", "0.03"]
```

```sh
cargo run -p cli -- sweep --strategy sma_trend --params grid.toml --symbol BTCUSDT \
    --interval 4h --from 2022-01-01 --to 2026-11-01 --slippage-bps 5 \
    --train-days 365 --test-days 90 [--objective sharpe|return|calmar]
```

It takes every `backtest` flag. The report lists each window's pick, its
train score, and its test return and drawdown next to the *median*
combination's test return, then the out-of-sample results chained. If
the pick doesn't beat the median, choosing parameters added nothing over
choosing at random. The run above (measured 2026-11-23, model output):
the picks chained to +106%, the median combination to +110%, and the
picks' mean return fell from +38.5% on train windows to +7.0% on test
windows. sma_trend's parameters, tuned on the past year, don't carry
forward. Each window is its own backtest from `--initial-cash`: a holding
is marked at the window's last close, not sold, so the chained figure
approximates one continuous run. At most `--max-combos` (200)
combinations: each costs two backtests per window, and more of them make
a lucky fit likelier.

`--earn-ratio 0.1` holds 10% of the cash back, as a live run sweeping that
share into Binance Simple Earn would (a strategy sees the same refusals);
`--earn-apr 0.04` values its interest at an assumed 4% a year. Binance's
rates change daily and their history isn't published, so the report
labels the figure as an assumption.

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

Before it runs, a backtest warns about any bar whose price moved more
than 30% from the previous close or within the bar (warm-up included):
usually a split the history doesn't adjust for (eToro's isn't) or a bad
print. Nothing is corrected, since real crashes move that much too, so
check the series or pick a window without it.

### Perpetual futures (USDⓈ-M: backtest and paper)

`--venue binance-usdm` backtests Binance USDⓈ-M linear perpetuals
(`binance-usdm:BTCUSDT`, separate from the spot pair). `fetch-data` also
caches the funding history, the symbol's liquidation clearance fee and,
when `BINANCE_API_KEY` / `BINANCE_API_SECRET` are set, your account's
margin brackets (a signed call; `--brackets-from demo|testnet|live`
names the host your key belongs to, default demo). The backtest refuses
to run without brackets rather than guess them.

```sh
cargo run -p cli -- fetch-data --venue binance-usdm --symbol BTCUSDT --from 2026-01-01
cargo run -p cli -- backtest --venue binance-usdm --strategy buy_and_hold --params p.toml \
    --symbol BTCUSDT --from 2026-02-01 --to 2026-08-01 --fee 0.0005 --slippage-bps 2 \
    --leverage 3 --margin isolated
```

Positions are signed, fees are paid in USDT, and margin is moved, not
spent: `--leverage` sets how much of the wallet a position ties up
(refused above what the brackets allow at that size), funding is paid or
received every settlement, and a position whose liquidation price the
bar's low (long) or high (short) reaches is closed there, losing its
margin. Liquidation is checked on the venue's mark price, as Binance does:
`fetch-data` caches the mark-price bars and the backtest replays them (a
paper run fetches each bar's); without them the trade bar's range stands
in, a cautious reading, with a warning. Funding records from before about
2023 carry no mark, so the bar's price values those. A quote
budget is the position's notional: at 1x, leave room for the fee.

`run --paper` with `exchange.venue = "binance-usdm"` forward-tests the
same model on live data: Binance's futures klines and funding
settlements, the local perp broker filling at the next bar's open with
the `[paper]` costs and the `[perp]` leverage and margin mode. Nothing is
sent to Binance but data requests; the margin brackets are read from your
account (`[perp] brackets_from` names the network your key belongs to). The
wallet lives in memory, so every run starts flat. See
`config/examples/sma_long_short.toml`.

`[perp] fills = "venue"` sends the orders to `exchange.network`'s futures
exchange instead: `--paper` on testnet or demo (fake money), real money only
with the same three locks as spot. At start the bot checks the account is
in one-way mode (it never switches it), sets each symbol's margin type and
leverage, and reconciles its books (`data/<network>-usdm.db`). Binance is
the truth for the position: every bar the bot compares it with its books,
and adopts trades it didn't place (a liquidation, auto-deleverage or a
manual trade) so the two agree; a difference no trade explains stops the
run. Give the bot its symbols to itself. Stop orders go through Binance's
algo-order service and trigger on the last price, as in the backtest. Turn
off BNB fee burn (a BNB fee can't be valued exactly). On exit it cancels
its open orders, but positions stay open on Binance, where they can still
be liquidated.

The report gives final equity, realized P&L, lowest cash, maximum drawdown,
fill and closed-trade counts, win rate, Sharpe ratio and rejected signals,
all as exact decimals. The fill model is stated, not hidden; see
[doc/architecture.md](doc/architecture.md#backtest-fill-model). Two
approximations to keep in mind: instrument filters are today's, not
historical (on old, low prices today's coarser tick makes taker fills
slightly worse: SOL at about 1.84 in 2021 with a 0.01 tick costs up to
about 0.5% per fill), and a whitelist chosen today carries survivorship
bias.

### Hedges: spot and perpetual together

`--perp-symbol` adds USDⓈ-M perpetuals to a Binance spot backtest
(`--venue binance`), each side in its own wallet as on Binance: spot
trades from `--initial-cash`, the perpetuals from `--perp-cash`, with
`--leverage`, `--margin` and `--perp-fee` (quote, default 0.0005). Nothing
moves money between the two wallets, as the bot can't live. Fetch both
caches first; `funding_carry` is the strategy built for it.

```sh
cargo run -p cli -- fetch-data --symbol BTCUSDT --interval 4h --from 2021-06-01
cargo run -p cli -- fetch-data --venue binance-usdm --symbol BTCUSDT --interval 4h --from 2021-06-01
cargo run -p cli -- backtest --venue binance --symbol BTCUSDT --perp-symbol BTCUSDT \
    --strategy funding_carry --params p.toml --interval 4h --from 2022-01-01 --to 2026-11-01 \
    --initial-cash 1000 --perp-cash 500 --leverage 2 --fee 0.001 --slippage-bps 2
```

### eToro

`--venue etoro` backtests eToro instruments (`etoro:AAPL`, `etoro:BTC`, any
symbol eToro lists) as the real asset: long only, no leverage, fees in
USD. eToro serves data only to a keyed account, so `ETORO_PUBLIC_KEY`
(your application's API key) and `ETORO_PRIVATE_KEY` (your account's user
key; a demo key works) must be set even for `fetch-data`. Stock markets
close over weekends and holidays, so a series is cut as delisted only
after 120 hours without a bar. eToro publishes no tick or unit step and
volume is often missing, so `avwap` has no data there.

`--cfd` backtests the symbols as eToro CFDs instead, so a strategy can
short them (`long_short_momentum`, `tsmom_long_short`, `sma_long_short`).
It is modelled on eToro's terms and a demo probe: 1x only, a fee rate each
way (`--fee 0.0015` is eToro's 0.15%), and a position is closed where its
loss reaches half its margin (filled like a stop: at that price, or at the
open after a gap). Not modelled, so stated: dividends, which a short pays;
overnight fees (none on an easy-to-borrow short at 1x, but leveraged CFDs
pay them, which is why leverage above 1 is refused); and a 1x long is a
CFD here where eToro would buy the real share ($1 an order). Backtest
only: the bot doesn't place CFD orders yet. Measured (model output):
`long_short_momentum` on AAPL, JPM, META, MSFT, UNH, V, XOM and QQQ, daily,
walk-forward 2022–2026: the picks chained +10.7% out of sample, the median
parameters −6.1% (the pick beat the median in 9 of 19 windows): a weak,
noisy result on eight names.

```sh
make etoro-stocks-data                                    # SPY, QQQ, AAPL, MSFT, AMZN daily
make etoro-stocks-backtest FROM=2024-01-01 TO=2026-11-01  # examples vs buy-and-hold SPY
# or any symbol and strategy:
make backtest VENUE=etoro STRATEGY=sma_trend SYMBOL=AAPL.RTH INTERVAL=1d \
     FROM=2024-01-01 TO=2026-11-01 SLIPPAGE_BPS=5 FEE=0.0002 HISTORY_LEN=100 \
     PARAMS=config/examples/etoro/params/sma_trend-aapl.toml
```

`config/examples/etoro/` has the params, two configs for your demo account,
and what to know: eToro charges $1 per stock order (set `--fee` to $1 / your
order size), `.RTH` symbols trade regular hours and start in mid-2023,
plain symbols (`AAPL`, `SPY`) go back to 2018 daily.

## Console (web)

A local web page for writing script strategies, backtesting any
strategy against buy-and-hold, and writing a run config:

```sh
cargo run -p cli --features rhai -- console   # release binaries: trade-bot console
# open http://127.0.0.1:9186/
```

- **Strategies**: pick a built-in (its params start from
  `config/examples/<name>.toml`) or a Rhai script in `strategies/`; edit the
  script and its params, *Check* builds it as a run would (a script's
  `check_params` included), *Save* writes `strategies/<name>.rhai`.
  *New script* starts from a small template.
- **Backtest**: any series `fetch-data` cached, with fee, slippage
  (required) and starting cash; results beside buy-and-hold of a symbol you
  pick on the same dates and costs, with both equity curves. It runs the
  same code as `trade-bot backtest`, so the numbers match it exactly.
- **Check lookahead**: reruns the strategy with every candle after a cut
  point rewritten, at 10 cut points, and fails if any decision or fill up
  to the cut changed (the strategy read ahead). It says so when a pass is
  vacuous (the strategy never traded differently).
- **Check restart**: stops the run at up to 10 points where nothing is
  working, restarts it from its saved state, and fails if the fills
  change (its `save_state` / `restore` loses something). It says so when
  no restart happened while holding a position.
- **Run config**: the questions of `trade-bot config init`, one at a time
  (same code: answers replayed on the server each step), with *Back* and
  *Start over*. The result comes with its check, to *Download* or *Copy*,
  and *Use in backtest* fills the backtest form with its strategy, params
  and market (saying which series still need `fetch-data`). Nothing is
  written on the server; keys are never asked for, the check only names
  the ones the server's env file will need.
- **Safety**: loopback only (an SSH tunnel to reach it from elsewhere),
  reads no keys, places no orders. It refuses requests from other web pages
  (wrong `Host` or `Origin`, or a write that isn't JSON), and writes only
  `strategies/*.rhai`. One backtest runs at a time.

Not yet: starting and watching paper runs from the page.

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

A live run can keep a share of its quote cash in Binance Simple Earn
(`[earn] enabled = true`, `ratio = "0.1"` for 10%): every hour, while no buy
is working, it tops the Earn balance up or redeems it back to that share,
and strategies trade from the rest. Flexible products by default; a locked
product is only topped up, never redeemed early. Binance serves Simple
Earn on its live host only, so the config refuses it on testnet and demo.

When a coin the bot trades is scheduled for delisting, the bot stops
buying it at once and sells what it holds `delist_exit_hours` (default 24)
before the delisting, whatever the strategy does. A live run reads the
schedule from Binance every hour; Binance publishes it on its live host
only, so testnet and demo runs never see one. Every run also checks each
pair's trading status every minute: while Binance has a pair halted, the
bot sends no orders on it and multi-coin strategies carry on without it.

What a run does:

1. Checks the key with a read-only signed request before trading.
2. **Reconciles** with Binance. It records fills that happened while the bot
   was down, cancels orders it left open, and refuses to start if an order
   it placed under another database is still open. If that database is
   gone for good, `run --paper --cancel-orphans` cancels those orders and
   starts; anything they already filled stays in your account but outside
   the bot's books. Don't use it while another copy of the bot is running:
   it cancels that copy's orders. Orders other people placed are left alone
   and only logged.
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


**Watching a run.** With `[api] enabled = true` and `dashboard = true`, a
running bot serves a dashboard at <http://127.0.0.1:9185/>: the run, its
positions with value and P&L at the last close, open orders, the latest
bars, and recent trades and orders, updated the moment the bot handles
an event (it follows `/events`, a server-sent event stream). It is
read-only like the API behind it (nothing on it can place or cancel an
order), loopback by default, and asks for the token when one is set.
### eToro

`exchange.venue = "etoro"` trades through eToro's API: `run --paper` on
your **demo** account (`exchange.network = "demo"`), real money only with
the same three locks as Binance. eToro is a broker, not an exchange: a
buy opens a position (by amount or units), a sell closes positions, and
every order is accepted first and executed later, so the bot polls each
to its outcome; a late refusal (eToro's ~$10 minimum, a closed market)
drops the order unfilled. eToro charges its fee on top of a buy's amount:
about 1% each way on crypto, and $1 per stock order (seen on the demo
account: $2 round trip on a $20 trade). Bars are polled a few seconds
after each interval boundary; a closed market simply produces none. Give
the bot its symbols to itself: a sell closes the account's oldest
positions on that symbol, so the bot refuses to start, and stops while
running, when the account's units on a symbol differ from its books
(positions opened or closed by hand, or a stock split changing the
units). eToro's own rules for a partial close apply: it
must take at least eToro's minimum (about $10; smaller is refused before
sending) and leave at least $1 (a sell that would leave less closes the
whole position). eToro publishes no unit step; the bot uses the 6 decimals
every position eToro opened carried. eToro limits requests per key (about 60–120 a minute by kind); the bot
spreads them out (at most one a second per kind), resolves all symbols in a
few batched requests at start, and waits out and retries a "too many
requests" answer. Two bots on the same key share eToro's limit, so keep
them few.

Orders eToro can take, and how the bot maps them:

| Strategy order | On eToro | Note |
|---|---|---|
| Market buy / sell | Open a position / close positions | |
| Limit buy below the market | Market-if-touched (`mit`) at the limit | Executes at market once touched: can fill worse than the limit |
| Limit buy at or above the market | Market buy | eToro's `mit` above the market would wait for a rise, like a stop |
| Resting limit sell above the market | Take-profit on positions whose units add up exactly to the order | Partial quantities can't rest |
| Limit sell at or below the market | Market close | |
| Immediate-or-cancel limit, stops | Refused | eToro's `limitIOC` ignored its limit on the demo account |

Backtests with `--venue etoro` fill limits the same way (on a touch, with
slippage, never capped at the limit) and refuse the same orders.

Fees and take-profit closes come from eToro's private WebSocket feed: it
is the only place eToro reports a close's fee. If the feed is down, the
bot still settles fills from REST and logs the close fee as unknown. A
request whose connection fails before eToro answers is retried twice
with the same request id, which eToro deduplicates.

Stocks keep market hours. The bot asks eToro every minute whether each
instrument can be traded now. A closed market isn't treated as a halt:
eToro accepts orders while closed and fills them when the market opens
(the order shows as "waiting for market"), which is where a backtest
fills them too. So a daily strategy deciding after the close still
trades, at the next open; and a strategy over several instruments
(`etoro:AAPL` with `etoro:BTC`, say) stops waiting for a closed stock's
bars and keeps trading the ones that are open. `etoro:AAPL` trades eToro's extended
hours; `etoro:AAPL.RTH` regular hours only. Only USD-priced instruments are
supported (a London or Tokyo stock is refused). On a real account US
stocks need a W-8BEN on file with eToro; declare it with `[etoro]
w8ben_signed = true`, or the bot refuses them at start.

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

## Running as a service

To start a run at boot (or login) and restart it after a failure, install
its config as a service, run as your user: a launchd agent on macOS, a
systemd user unit on Linux.

```sh
# Keys go in a file only you can read, never in the config or service file:
printf 'ETORO_PUBLIC_KEY=...\nETORO_PRIVATE_KEY=...\n' > ~/.trade-bot.env && chmod 600 ~/.trade-bot.env

trade-bot service install --config config/etoro-demo.local.toml --env-file ~/.trade-bot.env
trade-bot service status trade-bot-etoro-demo
trade-bot service remove trade-bot-etoro-demo     # stops it (closes out) and deletes the file
```

- Run it from the directory the bot's `data/` should live in, with an
  installed or release binary (`--bin`): `cargo clean` deletes the ones
  under `target/`. `--dry-run` prints the file without installing.
- Restarted after a failure (a stalled feed, a crash), not after a clean
  stop; each start reconciles with the venue first. A tripped kill switch
  exits with code 3: systemd doesn't restart it; launchd retries once a
  minute and is refused until you clear the switch.
- An env file others can read is refused. A live config installs only with
  `--live --i-understand` typed on the install command, and then trades
  real money at every start without asking again.
- Linux: the unit starts at login; `loginctl enable-linger $USER` makes it
  start at boot. macOS: the agent starts at login. Windows: use the
  container.
- The templates are in `deploy/service/` for setting it up by hand.

### On a VPS (any provider)

[`deploy/vps/README.md`](deploy/vps/README.md) walks through a rented
server step by step: create it (Ubuntu 22.04+ / Debian 12+, non-US region
for Binance), paste [`deploy/vps/cloud-init.yaml`](deploy/vps/cloud-init.yaml)
as its user data or run `site/vps-setup.sh` by hand, log in as `trade-bot`,
add your keys to `~/.trade-bot.env` yourself, then `config init`,
`config check --env-file`, `service install`. The setup script creates the
user, installs the release, and enables start at boot; it never sees a key.

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
volume, or, if the old one is lost, start once with `--cancel-orphans`.

**2. On Render, still testnet.** The *Deploy to Render* button at the
top (or *New > Blueprint* with this repo) reads `render.yaml`. Then, in the dashboard:
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
| `make backtest` | Backtest (`VENUE`, `STRATEGY`, `SYMBOL`, `FROM`, `TO`, `PARAMS`, `FEE`, `INITIAL_CASH`, `HISTORY_LEN`; `SLIPPAGE_BPS` required) |
| `make etoro-stocks-data` | Cache daily eToro history for SPY, QQQ, AAPL, MSFT, AMZN (`ETORO_STOCKS` to change; eToro keys needed) |
| `make etoro-stocks-backtest` | Backtest the eToro stock examples against buy-and-hold SPY (`FROM`, `TO`) |
| `make run-paper` | Paper trade on the configured fake-money network |
| `make migrate-up` / `-down` / `-status` / `-fresh` | Database migrations (`DATABASE_URL`) |
| `make console` | The local web console (script editor and backtests) on <http://127.0.0.1:9186/> |

`run` also applies pending migrations to its own database on start.

## Project layout

```
crates/
  core/       money types, ids, Instrument, Candle, Order, Fill, Position
  strategy/   Strategy trait, Context, registry, built-in strategies
  exchange/   Provider / MarketData / Broker traits, paper brokers (spot, perpetual),
              Binance spot + USDⓈ-M, eToro, MAX
  engine/     the event loop shared by backtest and live, risk checks
  backtest/   kline cache, BacktestProvider, report
  storage/    SeaORM repositories: trades, holdings, order journal
  api/        read-only HTTP API: status, positions, orders, trades
  cli/        the `cli` binary (`trade-bot`): fetch-data, backtest, sweep, run, config, console, service
migration/    SeaORM schema migrations
strategies/   script strategies (Rhai), e.g. sma_trend.rhai
config/       config.example.toml, examples/ (one per strategy; etoro/ for stocks),
              delistings/ (announced delistings for backtests); real configs gitignored
doc/          architecture notes
deploy/       Prometheus, Alertmanager, Loki + Alloy, Grafana (compose) and alert rules
```

## Known limitations

- Live trading: Binance spot, and Binance USDⓈ-M perpetuals (one-way mode,
  market and limit orders; not yet run with real money). eToro: the real
  asset, long only, USD-priced instruments, market orders plus
  market-if-touched buys and take-profit sells (demo-tested; not yet run
  with real money). MAX is paper only; options are planned but not
  implemented.
- A market-data outage longer than `exchange.max_outage_secs` (default 10
  min) stops the run. Bars missed during a shorter one are backfilled as
  history; only the newest is traded on.
- Strategy state is saved per strategy and restored only under the same
  params; changing a strategy's params (say, its coin list) starts it fresh.
- The backtest broker trades from `--initial-cash` in one quote asset and
  refuses buys it can't cover, as Binance would. A buy placed right after
  a market sell may count that sale's expected proceeds (last close); at
  the next open it fills after the sale, and if the sale raised less than
  the buy costs, the buy is dropped unfilled (live: `-2010`).
- BNB fees are valued at the BNB price in the trade's minute, an
  approximation, because Binance doesn't report its conversion rate.
- Stock splits and other corporate actions are not handled. eToro's price
  history is not split-adjusted (and mixes price scales around some
  splits), so a backtest across a split is wrong. Live, eToro adjusts the
  position's units and the books can't follow: the run stops at the next
  bar (the books no longer match the account), and the position must be
  sorted out by hand before restarting.
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
and publishes the archives with `SHA256SUMS` as a GitHub release:
`trade_bot_<os>_<arch>.<version>.tar.gz` (`linux_amd64`, `linux_arm64`,
`darwin_arm64`, `darwin_amd64`; Windows as `trade_bot_windows_amd64.<version>.zip`).
*Run workflow* on the Release action builds the archives without
publishing.
`pages.yml` publishes `site/` (a single static page) to
<https://sc0vu.github.io/trade-bot/> on every push to `main` that touches it.

## License

MIT. See [LICENSE](LICENSE).
