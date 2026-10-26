# Architecture

This document explains how the bot is put together and why. The rules it
relies on are enforced by code and tests; where a rule matters, the test
that pins it is named.

## Contents

- [Design goals](#design-goals)
- [Data flow](#data-flow)
- [Crates](#crates)
- [Core types](#core-types)
- [The engine loop](#the-engine-loop)
- [Providers, feeds and brokers](#providers-feeds-and-brokers)
- [Backtest fill model](#backtest-fill-model)
- [Backtest report](#backtest-report)
- [Binance](#binance)
- [Books: journal, ledger, holdings](#books-journal-ledger-holdings)
- [Restarts](#restarts)
- [Reconcile](#reconcile)
- [Shutdown](#shutdown)
- [Observability](#observability)
- [The live gate](#the-live-gate)
- [Configuration](#configuration)
- [Strategies and how they are loaded](#strategies-and-how-they-are-loaded)
- [Where the design is headed](#where-the-design-is-headed)

## Design goals

1. **Backtest equals live.** One `Strategy` trait, one engine loop. A
   backtest swaps only the provider: cached candles plus a simulated broker,
   instead of a WebSocket feed plus a real one. That identity is the whole
   correctness story. A backtest is only evidence about live behaviour if
   the code paths are the same.
2. **Correct money.** Every price, quantity, balance and fee is an exact
   decimal. Binary floats lose fractions of a cent, and those losses
   compound over thousands of trades.
3. **No lookahead, by construction.** The engine is event-driven: a
   strategy is called once per event and can see only what has already
   happened. A vectorized design over a full price table would make peeking
   at future rows easy and invisible.
4. **Safe by default.** Every default points at fake money. Real money needs
   three independent deliberate steps.
5. **Room to grow.** Venues, asset classes and strategy-loading mechanisms
   can be added without reshaping what exists.

## Data flow

Data flows one way. A strategy only decides; the provider is the only part
that differs between backtest and live.

```
Provider (BinanceSpot | Backtest) ── supplies ──> MarketData + Broker

MarketData ──MarketEvent──> Engine loop ──> Strategy.on_event() ──Signal──> Risk ──Order──> Broker
                                 ▲                                                          │
                            Context (positions, history, open orders, params) <───Fill──────┘
                                                                                     └─> Books (SQLite / MySQL)
```

## Crates

| Crate | Role | Depends on |
|---|---|---|
| `core` (package `bot-core`) | Money newtypes, ids, `Instrument`, `Candle`, `Order`, `Fill`, `Position`, `MarketEvent`, `Signal`. No I/O. | — |
| `strategy` | `Strategy` trait, `Context`, registry, built-in strategies | core |
| `storage` | Repository traits and their SeaORM implementation | core, migration |
| `exchange` | `Provider` / `MarketData` / `Broker` traits, the paper broker, the Binance client | core, storage |
| `engine` | The event loop and risk checks | core, strategy, exchange |
| `backtest` | Kline cache, `BacktestProvider`, report | core, engine, exchange, strategy |
| `api` | Read-only HTTP API over live state and the books | core, engine, storage |
| `cli` | The binary: config, the live gate, signals, metrics, the API | everything |
| `migration` | Schema migrations | sea-orm-migration |

## Core types

**Money.** `Price`, `Qty` and `Notional` wrap `Decimal`. There is
deliberately no `Qty * Price`: converting units is an instrument's job
(below). Serde reads them only from strings, because a JSON or TOML number
would become a float before `Decimal` saw it.

**Ids.** `InstrumentId` is `venue:SYMBOL` (`binance:BTCUSDT`), with case
normalized.

**Validated types.** `Instrument`, `Candle`, `Quote`, `Order` and `Fill` are
built only from a `*Spec` struct through a checking constructor, and expose
their fields read-only. Any value of these types has passed validation.

**Instrument.** All asset-class arithmetic lives here: `notional`,
`qty_for`, `settlement`, `round_qty` (toward zero, to the lot),
`round_price` (toward the passive side, to the tick), and `check_order`
(the venue's filters). A broker submits orders and reports fills; it never
values anything. When equities and options arrive, their multipliers and
settlement rules go here and callers don't change.

**Fees.** `Fee::Base`, `Fee::Quote` or `Fee::Other { asset, amount, value }`.
Binance charges a buy's fee in the base asset, a sell's in the quote asset,
or both in BNB when the BNB discount is on. `value` is a third-asset fee
converted to the instrument's quote asset; an unvalued `Other` fee is
rejected by position accounting rather than ignored.

**Position.** Average cost, fees included. The total `cost_basis` is stored
and the average derived, so no rounding accumulates. v1 positions are
non-negative spot inventory, but the sign is a runtime policy
(`Instrument::allows_short`, checked by `Position::check_against`), not a
type rule. Short and perpetual positions in v2 can reuse the type.

**MarketEvent.** `Candle`, `Quote`, `SessionOpen`, `SessionClose`,
`CorporateAction`, marked `#[non_exhaustive]`. v1 emits only candles; the
other variants exist so that adding equities' session boundaries and
corporate actions is additive. A candle's timestamp is its close time,
because that is when the bar is known.

## The engine loop

`engine::run(provider, strategy, params, config)` runs until the feed ends
or something fails. Before the first event, the context gets the broker's
position for every whitelisted instrument (always flat in a backtest; a
live broker's stored holdings), and then `on_start` runs. For each event:

1. **Order check.** Event time must not go backwards, or the run stops.
2. **Settle.** The broker settles pending orders against the *new* event.
   An order decided on an earlier event fills no earlier than this one.
3. **Fills.** Open orders are refreshed from the broker. For each fill, the
   context's position is updated and `on_fill` runs, *before* the event
   joins the history: a fill at a candle's open must not see that candle's
   close.
4. **Decide.** The event joins the history (at most `history_len` closed
   candles per instrument) and reaches `on_event`.
5. **Route.** Signals go through risk to the broker, stamped with this
   event's time.

Risk (v1) checks the whitelist, sizes orders (converting a quote amount to
base at the order price, except a market buy, which keeps its quote budget),
rounds to lot and tick in the direction that is never more aggressive, and
applies `check_order`. Order ids are deterministic (`o-1`, `o-2`, …), so two
runs over the same data produce identical orders.

**Errors.** A risk refusal, or a venue refusing *one order* (paper refusals,
an unknown order, Binance `-1013` filter failure, `-2010` order rejected,
`-2011` cancel rejected), is recorded in the run report and the run
continues. Every other broker error stops the run: bad keys (`-2015`), clock
skew, rate limits, decode failures, and transport errors, where a submit may
have reached the venue and its state is unknown. A run that kept going
through those would look healthy while trading nothing.

Pinned by `crates/backtest/tests/lookahead.rs`: for every cut point *t*,
all candles after *t* are rewritten, and every fill and rejection up to *t*'s
close must be identical. It runs for `ratio_hop` and `sma_trend`.

## Providers, feeds and brokers

```rust
trait Provider {
    fn id(&self) -> &str;
    fn asset_classes(&self) -> &[AssetClass];       // the engine refuses mismatches up front
    async fn market_data(&self) -> Result<Box<dyn MarketData>>;
    async fn broker(&self) -> Result<Box<dyn Broker>>;
    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>>;
}
trait MarketData { async fn next(&mut self) -> Result<Option<MarketEvent>>; }
trait Broker {
    async fn submit(&mut self, o: Order) -> Result<OrderId>;
    async fn cancel(&mut self, id: OrderId) -> Result<()>;
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>>;
    fn position(&self, i: &InstrumentId) -> Position;
    fn open_orders(&self) -> Vec<Order>;
    fn cancelling_orders(&self) -> Vec<Order>;  // cancelled, may still fill; default none
}
```

| | Backtest | Live |
|---|---|---|
| Provider | `BacktestProvider` | `BinanceSpotProvider` |
| Feed | Cached klines, merged across instruments by (close time, id) | `KlineStream`: Binance WebSocket, closed bars only |
| Broker | `PaperBroker` with a pluggable `FillModel`, `SlippageModel`, `FeeModel` | `BinanceBroker` over signed REST |

The paper broker and the Binance broker refuse the same things before
submitting (unknown instrument, reused id, selling more than the holding
minus pending sells), so a strategy sees identical refusals in both.

## Backtest fill model

A candle carries four prices and can't say whether its high or its low came
first. `NextOpenFillModel`, the default, settles that ambiguity the same way
every time:

- An order is evaluated only on candles that open after it was created.
- **Market:** fills at the next candle's open, as a taker.
- **Limit buy:** on its first candle, if the open is strictly below the
  limit, fills at the open as a taker. Otherwise it rests, and fills at the
  limit as a maker only if price trades *through* it (`low < limit`).
  Touching exactly the limit doesn't fill, because queue position is
  unknown. Sells mirror this. IOC/FOK: the first open, or expire. No partial
  fills.
- **Stop:** triggers at `high >= stop` (buy) or `low <= stop` (sell).
  Stop-market fills at the stop, or at the open if price gapped through.
  Stop-limit never fills on its trigger candle (the worse case), and from
  the next candle on behaves as a limit.
- **Quote-budget market buy** (Binance `quoteOrderQty`): sized at the
  slipped fill price to the largest lot multiple whose notional plus quote
  fee fits the budget. It never spends more than the budget.
- **Fees** apply to every fill, in the received asset like Binance, or in
  quote. **Slippage** (in bps) applies to taker fills only, rounded to the
  tick against the trader, and is capped at the limit for a limit order.
- If a stop and a target fall in one candle, the model can't know which came
  first, and it assumes the worse.

The paper broker refuses sells beyond the holding minus pending sells. In
a backtest it also trades from the initial cash and refuses, at submit, a
buy it can't cover, as Binance does. Each pending buy reserves the most it
can spend (its quote budget, its notional at the limit, or a market buy's
notional at the last close plus slippage). A pending market sell's
expected proceeds count as available: live, a market sell fills at once,
so a buy placed right after it can spend them, and rotating strategies
rely on that. When a buy fills, its actual cost is checked again, and if
it no longer fits (the sale raised less, or was cancelled) the buy is
dropped unfilled, so cash never goes negative. A finer
`VolumeFillModel` (capping fills at a share of bar volume) can be plugged in
later; the engine and strategies never see which model is in use.

## Backtest report

All figures are exact decimals:

- **Equity** is cash plus holdings at the latest close, once per close time.
  The report keeps its own cash ledger by replaying fills.
- **Maximum drawdown** is a fraction of the running peak, which starts at
  the initial cash.
- **`min_cash`** is the lowest cash after any fill. The report's ledger is
  independent of the broker's balance, so a negative value would expose a
  broker that let an overspend through.
- **A closed trade** is a sell, and it counts as a win if its realized P&L
  is positive.
- **Sharpe** is the mean over the sample standard deviation of per-bar
  equity returns, times √(bars per year) (24/7 markets: 8760 at 1h), with a
  zero risk-free rate. It is `None` with fewer than two returns or zero
  deviation.

Regression tests (`crates/backtest/tests/ratio_hop*.rs`, `sma_trend.rs`)
replay small hand-built fixtures and assert values worked out by hand, so a
refactor can't silently change P&L.

## Binance

**Networks.** `exchange.network` selects the hosts:

| Network | REST | WebSocket |
|---|---|---|
| `testnet` | `testnet.binance.vision` | `stream.testnet.binance.vision` |
| `demo` | `demo-api.binance.com` | `demo-stream.binance.com` |
| `live` | `api.binance.com` | `stream.binance.com:9443` |

Historical data for backtests comes from the keyless `data-api.binance.vision`.

**Signing.** HMAC-SHA256 over the query string, with the server-time offset
measured at start so a skewed local clock doesn't get every request
rejected. HTTP and the clock sit behind traits (`Transport`, `Clock`), so
tests never touch the network. The signer reproduces the example signature
in Binance's documentation. Every request is bounded by
`exchange.http_timeout_secs`.

**Rate limits.** Binance allows 6000 request weight per IP per minute and
100 orders per 10 seconds (200,000 per day) per account, and bans an IP that
keeps exceeding them. Every REST call first waits in
`exchange::rate_limit::RateLimiter` until it fits 80% of each limit, so other
programs on the IP keep some room. It is a sliding-window log, not a leaky or
token bucket: Binance's windows are fixed, and a bucket holding the whole
budget can spend it just before a window resets and again just after, nearly
twice the limit in one Binance minute; the log keeps every span of the
window within budget. Weights per endpoint (`binance::limits`) were measured
from the `X-MBX-USED-WEIGHT-1M` header; that header, reported on every
response, also corrects the count when it is higher (something else using
the IP). A 429 or 418 holds every request, the exit close-out's included,
for `Retry-After`; the error itself still stops the run. One limiter is
shared by all of a provider's clients, since the weight is counted per IP.

**Startup checks.** A read-only signed `GET /api/v3/account` must succeed
and report `canTrade` before any order is placed.

**Orders.** A client order id is a per-run tag plus the engine's id
(`tb<start time in base 36>-o-N`). Engine ids restart at `o-1` each run, and
Binance keeps client ids unique only among open orders, so the tag keeps
them unique across runs and marks the bot's own orders. After submit, status
and cancel go by Binance's `orderId` from the submit reply, never by client
id. A quote budget is sent as `quoteOrderQty`, rounded toward zero.

**Feed.** The kline WebSocket runs inside `feed::ReconnectingFeed`. On first
connect it fetches the last `history_len` closed bars per coin over REST as
`MarketEvent::Backfill`, so indicators start full after a restart. When the
socket drops or stalls it reconnects with backoff (1 s doubling to 60 s) for
up to `exchange.max_outage_secs`, then backfills the bars it missed: all but
the newest per coin as `Backfill` (history only, since deciding on them
would trade on stale prices), the newest as `Candle`. It subscribes before
backfilling so nothing closes unseen in between, and drops bars it already
emitted. Missed bars are merged by (close time, instrument), as the backtest
orders them.

**Fills.** `settle` polls each open or cancelling order's status and its
`myTrades`. An order is dropped only when Binance reports it closed *and* its
trades add up to `executedQty`, since the two endpoints can lag each other.
Fills are polled once per candle, the same timing as the paper broker.

**BNB fees.** A fee in a third asset is valued at the `close` of the
one-minute `{asset}{quote}` kline (e.g. BNBUSDT) containing the trade,
using that pair's `Instrument::notional`. REST klines include the minute
in progress, so a fee valued during its minute (usual when live) gets the
latest price then; one valued after (restart reconcile) gets the final
close. `myTrades` doesn't report
Binance's own rate, so this is an approximation of at most a minute's BNB
price move on a fraction-of-a-percent fee. A missing pair or candle is an
error, never a guess.

## Books: journal, ledger, holdings

A live broker keeps its books through three storage traits, combined as
`exchange::Books`; `storage::SqlStore` implements all three:

- **`OrderJournal`** (`orders` table). A row is written as `pending` *before*
  the submit request goes out, becomes `open` with Binance's id on the ack,
  and becomes `closed` only after the order's last fill is recorded. A crash
  at any point leaves either a closed row or one worth checking.
- **`Ledger::record_venue_fill`**. One transaction appends the trade and
  folds it into the holding with `Position::apply`, so storage computes the
  position and a caller can't pass one in that has drifted. It is idempotent
  on the venue's trade id (unique per instrument), so replaying Binance's
  trade history never counts a fill twice. MySQL uses `FOR UPDATE`. Each
  trade also keeps its journal order's run-tagged `client_order_id`, so a
  trade can be joined to its `orders` row across runs: the fill's own
  `order_id` is the engine's, which restarts at `o-1` every run. Rows
  recorded before that column existed have none; no exact link exists for
  them, so none is guessed.
- **`HoldingRepository`**. The current position per instrument, a cache of
  the trade log. The broker's positions *are* these holdings, so they
  survive restarts.

Money columns are exact-decimal TEXT, never `DECIMAL`: SeaORM on SQLite
creates `DECIMAL` as `real` and reads it through `f64`. The cost is no SQL
`SUM` or numeric `ORDER BY` on money; aggregation happens in Rust. Reads go
back through the core constructors, so a corrupt row is an error.

Each network has its own database (default `data/<network>.db`), opened with
pending migrations applied and stamped with its network on first use.
Opening it for another network fails, so fake fills never count as real
holdings.

## Restarts

A restart should carry on, not start over. Three things make that work:
the books (positions and the order journal) survive in the database; the
feed warms the strategy's history up with the last `history_len` bars as
`Backfill`; and each strategy's own state is saved after every event
(`Strategy::save_state`, written by a background task to `strategy_state`)
and restored at the next start (`restore_state`), checked against the books.
Saved state applies only under the same params (a SHA-256 of the strategy's
name and params). Fills that happened while the bot was down are recovered by
the startup reconcile and delivered to `on_fill` with the first decision
event, so, for example, `ratio_hop` restarted between the legs of a rotation
buys with the proceeds of the sell that filled meanwhile.

## Reconcile

Binance is the source of truth for what happened; the books say which
orders and fills are the bot's. `binance::reconcile` brings them back in
line. It runs in `provider.broker()` before trading, and again however a run
ends.

For every order the journal hasn't closed:

1. Find it on Binance by venue id, or by client id if the submit reply was
   lost. A pending order Binance has never heard of (`-2013`) was never
   placed, and is closed.
2. Cancel it if it is still working. Strategy state isn't persisted, so a
   leftover order has no strategy to manage it.
3. Wait until Binance reports it closed with every executed unit visible as
   a trade (10 polls, 500 ms apart). Otherwise stop and say so.
4. Record every trade (idempotently), then close the row.

At startup only, it also:

- Refuses to start if an open order with the bot's prefix is missing from
  this database: the bot ran under another database, and that order's fills
  would never reach these books. Orders other people placed are only
  logged. The books count only the bot's own fills, and at worst such an
  order uses balance a bot order needed, which Binance then refuses with
  `-2010` (a non-fatal rejection).
- Refuses books that hold more of a coin than the account does (free plus
  locked). Less is fine: the account may hold coins the bot never bought.

## Shutdown

SIGINT (Ctrl-C), SIGTERM (`kill`, `docker stop`, systemd) and SIGHUP (the
terminal closing) all stop trading and run the exit reconcile, which cancels
the bot's open orders. A second signal during that cleanup abandons it and
exits non-zero, so a person can always get the process back; the next
start's reconcile finishes the job. Holdings are kept, not sold.

A crash or `kill -9` can't be handled in-process. The orders stay live until
the next start, so a long-running bot belongs under a supervisor that
restarts it.

## Observability

The engine and the Binance client record metrics through the `metrics`
facade. Nothing is recorded unless the binary installs a recorder, so
backtests and tests pay nothing and stay deterministic. Labels are bounded
(instruments, sides, endpoint paths, error codes), never order ids or
amounts.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `tradebot_events_total` | counter | | Market events processed |
| `tradebot_last_candle_close_seconds` | gauge | `instrument` | Close time of the latest candle; `time() - x` is the feed's age |
| `tradebot_orders_submitted_total` | counter | `instrument`, `side` | Orders the broker accepted |
| `tradebot_cancels_requested_total` | counter | | Cancels the broker accepted |
| `tradebot_fills_total` | counter | `instrument`, `side`, `liquidity` | Fills |
| `tradebot_signals_rejected_total` | counter | `source` (`risk`/`broker`), `code` | Signals that didn't become orders |
| `tradebot_binance_request_seconds` | histogram | `method`, `path` | REST request time |
| `tradebot_binance_errors_total` | counter | `path`, `code` | Failed REST requests, by Binance code or `http` |
| `tradebot_reconcile_orders_closed_total` | counter | `phase` | Journal orders reconcile closed |
| `tradebot_reconcile_fills_recovered_total` | counter | `phase` | Fills reconcile recorded that the books lacked |

Live state beyond counters reaches the outside through
`engine::run_observed`: the same loop as `run`, handing an `Observer` a
`Snapshot` (positions, open orders, last candle per instrument, counts) after
every event. The observer can't change the run and must return quickly;
plain `run` builds no snapshot at all.

`run` exports all of this when `metrics.enabled` is set: a Prometheus
endpoint at `metrics.listen` (default `127.0.0.1:9184`), process metrics
(memory, CPU, open files) sampled every 15 s, and gauges from the observer:
open orders and the last close per instrument always, and position quantity,
cost basis and realized P&L only with `metrics.include_money`. The endpoint
has no authentication, so a non-loopback address needs
`metrics.allow_public`. If the exporter can't start, the run logs a warning
and trades on.

With `api.enabled`, the same observer feeds the read-only HTTP API (`api`
crate) through a `watch` channel, which never makes the engine wait:
`/status` returns the latest snapshot with network, strategy and start time;
`/positions`, `/orders` and `/trades` read the books. There are only `GET`
routes and none that changes anything; later controls are limited to
pausing, cancelling and stopping, never placing orders or loosening the live
gate. Off loopback the API needs `api.allow_public` and a bearer token from
`TRADEBOT_API_TOKEN`, compared in constant time.

Metric values are `f64`, converted at the moment of recording. They are for
display and alerting only and are never read back into a decision or the
books, so invariant 1 still holds where money is decided.

## The live gate

`cli::gate::decide` is a pure function, tested against every combination of
its inputs:

- Without `--live`: the configured fake-money network only. `mode = "live"`
  or `network = "live"` in config is refused rather than quietly downgraded,
  so the mismatch gets noticed.
- With `--live`: all three of `mode = "live"`, `exchange.network = "live"`
  and `--i-understand` are required, and every missing one is listed.
- A fake-money run refuses `rest_url` / `ws_url` overrides that point at a
  real-money `*.binance.com` host (only the two demo hosts are allowed),
  since an override could otherwise bypass every lock.

## Configuration

`figment` layers built-in defaults, the TOML file (`config/config.toml`,
`--config` or `BOT_CONFIG`), and `BOT_*` environment variables (nested with
`__`) into one typed struct with `deny_unknown_fields`. A typo is an error,
not a silent default. Validation runs on load, so a bad config never
reaches the trading loop. API keys are the exception to the file layer:
they are read only from `BINANCE_API_KEY` and `BINANCE_API_SECRET`, and no
field exists that could hold one. `config/config.example.toml` documents
every field, and a test loads it.

## Strategies and how they are loaded

Python can import a `.py` file at runtime. Rust is compiled, so "drop in a
strategy" has several real answers:

| Approach | Recompile host? | Sandboxed | When |
|---|---|---|---|
| Registry (`inventory`) | yes | n/a | v1: strategies in this repo |
| `cdylib` | no | no (no stable ABI) | rarely |
| Rhai script | no | yes | external authors, candle frequency |
| WASM (`wasmtime`) | no | yes | external authors who need speed |

v1 uses the registry: a strategy file calls `inventory::submit!` and is
available by name, with no central list. Because a strategy's only window to
the world is `Context`, the loading mechanism can change later without
touching strategy code.

## Where the design is headed

The scope ladder is **v1:** crypto spot, then US equities. **v2:** options
and perpetual futures.

- **Equities** add a provider (e.g. Alpaca), `AssetClass::UsEquity`, T+1
  settlement in `Instrument`, and the session and corporate-action events
  that already exist as `MarketEvent` variants.
- **Perpetual futures** break the "owned inventory" assumption: signed
  positions, funding payments, mark-to-market P&L and broker-initiated
  liquidation. That is a `Position` model change and gets its own effort.
  Half-built margin accounting would be worse than none, so v1 deliberately
  has none.
