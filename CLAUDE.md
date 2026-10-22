# CLAUDE.md

## Project
Rust trading bot. First venue Binance (crypto spot); multi-venue, multi-asset by design so equities follow without rewrite.
Event-driven, strategy-pluggable, backtester runs **same** strategy code as live. v1 scope: paper/backtest-correct on crypto spot; real
orders gated behind explicit config.

Scope ladder (build in order, don't foreclose later rungs):
**v1: crypto spot now → US equities next. v2: options & perpetual futures.**
Domain split so equities/options/perps *can* slot in; implement only crypto spot now.
Perps (v2) break "owned inventory" assumption — signed positions, funding
payments (recurring cash flow), mark-to-market unrealized PnL,
broker-initiated liquidation. `Position`-model change, not just new
`AssetClass` variant — dedicated effort like options, not flag. Don't
half-build in v1: partial margin accounting that looks working worse
than none.

Built with Claude Code. Owner reasons from first principles: on design choice, name governing constraint (fees, latency, lookahead,
precision, settlement), don't copy other bot. Don't take strategy's stated edge at face value — derive why it holds or not after costs.

## Non-negotiable invariants
Correctness, not style. Task violates one → stop, flag.
1. **Money is `rust_decimal::Decimal`.** Never `f64`/`f32` for price, quantity,
   balance, fees. Binary float silently loses cent fractions, compounded
   over thousands of trades. Wrapped in unit newtypes `Price`, `Qty`,
   `Notional` (no bare-`Decimal` money); `Qty * Price` deliberately absent —
   unit conversion only via `Instrument` (invariant 4). Serde reads them
   only as strings: TOML/JSON number → float before `Decimal` sees it.
2. **No lookahead.** Strategy reads market data only up to and including
   current event. Never index future bar. *Why* engine
   event-driven, not vectorized over full DataFrame.
   Pinned by `crates/backtest/tests/lookahead.rs`: rewrite all candles after
   t → every fill/rejection up to t's close must be identical.
3. **Backtest == live.** `Strategy` trait and engine loop shared.
   Backtest swaps only provider (data feed + broker). Change makes
   backtest and live diverge in strategy-visible behavior → change wrong.
4. **Asset-class math lives in `Instrument`, not in `Broker` or `Strategy`.**
   Notional, settlement, multiplier, expiry belong to instrument. Broker
   submits orders, reports fills; never values one.
5. **A strategy's only window to the world is `Context`.** Position, rolling
   history, params, submit-order — never direct handle to broker, provider,
   network. Lets strategy-import mechanism evolve (registry →
   script → wasm) without touching strategy code.
6. **Secrets via env only.** API keys from env vars named in config; never
   hard-code or commit. `.env` and `config/*.local.toml` gitignored.
7. **Safe by default.** Default mode paper against testnet. Live
   requires `mode = "live"`, `exchange.network = "live"`, AND explicit
   `--i-understand` run flag. Three locks — mistake spends real money.

## Architecture
One-way data flow. Strategy pure decision; provider only thing
differing backtest vs live.

```
Provider (BinanceSpot | Alpaca | Backtest) ── supplies ──> MarketData + Broker
         │
MarketData ──MarketEvent──> Engine loop ──> Strategy.on_event() ──Signal──> Risk ──Order──> Broker
                                   ▲                                                           │
                              Context (position, rolling history, params) <────────Fill───────┘
                                                                                         └─> Storage (SeaORM)
```

**Provider** = venue + capabilities. Mints `MarketData` feed and
`Broker`, declares served asset classes, so engine
rejects equity strategy on crypto-only venue before trading:

```rust
#[async_trait] trait Provider {
    fn id(&self) -> &str;
    fn asset_classes(&self) -> &[AssetClass];           // Spot now; UsEquity later
    async fn market_data(&self) -> Result<Box<dyn MarketData>>;  // live connect can fail
    async fn broker(&self) -> Result<Box<dyn Broker>>;
    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>>; // reference data; unknown id = error
}
```

**Instrument** carries asset-class math (invariant 4). v1 implements
`Spot` only; enum leaves room for `UsEquity` (later `UsOption`) without
reshaping callers:

```rust
enum AssetClass { Spot, UsEquity /*, UsOption (deferred) */ }

struct Instrument { id: InstrumentId, class: AssetClass, /* base/quote, tick, lot, min_qty, min_notional */ }
impl Instrument {
    fn notional(&self, qty: Qty, price: Price) -> Notional;   // qty*price now; *multiplier for options later
    fn qty_for(&self, n: Notional, price: Price) -> Result<Qty, FilterError>;
    fn settlement(&self) -> Settlement;                        // Instant for spot; T+1 for US equity
    fn round_qty(&self, q: Qty) -> Qty;                        // toward zero, to lot
    fn round_price(&self, p: Price, side: Side) -> Price;      // toward passive side, to tick
    fn check_order(&self, q: Qty, p: Price) -> Result<(), FilterError>; // venue filters
}
```

`InstrumentId` = `venue:SYMBOL` (`binance:BTCUSDT`), case-normalized.
Validated core types (`Instrument`, `Candle`, `Quote`, `Order`, `Fill`) built
only from `*Spec` via checking `new`/`try_from`; fields via `Deref`. Existing
value passed validation. `Fill.fee` is `Fee::{Base, Quote,
Other}` — Binance charges buy fee in base, sell in quote, or BNB; v1 position
accounting counts `Other { value: Some(v) }` like quote fee `v` (third-asset
fee valued in quote at fill time by venue broker), rejects `value: None`.
Positions average cost, fees included;
store total `cost_basis`, derive average (no accumulated rounding).
v1 positions non-negative owned inventory (spot). Quantity sign = value
concern, not compile-time invariant — v2 signed (short/perp) position
must reuse type, not force rewrite. Don't encode `quantity ≥ 0` as
type-level law (no unsigned/`NonZero` qty, no `try_from` rejecting negatives).
Sign = runtime policy: `Instrument::allows_short()` (false all v1 classes),
checked by `Position::check_against(&Instrument)`; `from_parts` checks only
sign-agnostic consistency (basis sign = qty sign, flat ⇒ zero basis).

Strategy reacts to **MarketEvent**, not only candle, so loop
survives non-price events equities/options bring (session boundaries,
corporate actions, expiry). v1 emits `Candle` only; other variants exist so
later adding additive, not rewrite:

```rust
#[non_exhaustive]  // strategies keep fallback arm; engine matches all
enum MarketEvent { Candle(Candle), Quote(Quote),
                   SessionOpen { venue: VenueId, ts: Timestamp },   // carry ts: orderable
                   SessionClose { venue: VenueId, ts: Timestamp },
                   CorporateAction(CorpAction) /*, Expiry(InstrumentId) deferred */ }
// CorpAction is empty enum in v1 → variant unconstructible until equities.
// MarketEvent::ts(): candle → close_time (bar known), never open_time.

trait Strategy: Send {
    fn name(&self) -> &str;
    fn on_start(&mut self, _ctx: &mut Context) {}
    // default: Candle → on_candle, other events ignored
    fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal> { .. }
    fn on_candle(&mut self, _ctx: &mut Context, _c: &Candle) -> Vec<Signal> { vec![] }
    fn on_fill(&mut self, _ctx: &mut Context, _f: &Fill) {}  // signals via ctx.submit
}
// Context: now, instrument(s) (universe = resolved whitelist), position,
// open_orders (ids for Signal::Cancel; as of last settle),
// history (≤ history_len closed candles, oldest first, ends at current event),
// params, submit. Engine mutates via `strategy::driver` (strategy calling it
// only corrupts own view; broker = source of truth).

#[async_trait] trait MarketData { async fn next(&mut self) -> Result<Option<MarketEvent>>; } // feed error stops run
#[async_trait] trait Broker {
    async fn submit(&mut self, o: Order) -> Result<OrderId>;
    async fn cancel(&mut self, id: OrderId) -> Result<()>;
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>>; // pending vs NEW event; live: drain venue fills
    fn position(&self, i: &InstrumentId) -> Position;
    fn open_orders(&self) -> Vec<Order>;  // snapshot → ctx after each settle
}
```

**Fills are a swappable `FillModel` inside the broker (LEAN-style).** PaperBroker
holds `FillModel` + `SlippageModel` + `FeeModel` as trait objects; default
`NextOpenFillModel` implements fill-model section rules. Backtest and
live differ only by plugged fill model — live's "model" = real
exchange. `VolumeFillModel` (cap fill at share of bar volume) later
for low-volume whitelist. Engine and strategy never see this.

Engine generic over `Provider`. Backtest = `BacktestProvider`
(kline replay + PaperBroker). Live = `BinanceSpotProvider` (ws + BinanceBroker).
One loop, one strategy, provider swapped — that identity = whole correctness
story. Backtest must replay same `MarketEvent` stream live produces, so parity
holds for every event type, not just candles.

Loop order matters: each event, broker settles pending orders against
NEW event *before* strategy runs, so order decided at t fills
at t+1. Prevents deciding on close and trading at it.
`engine::run` per event: (1) feed ts must not go backwards → else stop;
(2) `broker.settle(event)`; (3) each fill → ctx position from broker →
`on_fill`, *before* event joins history (fill at open must not see that
close); (4) event → history → `on_event`; (5) signals → `engine::risk` →
broker, `created_at` = event ts. Risk v1: whitelist, sizing (quote → base
at order price, except market buy keeps quote budget), lot/tick rounding
(never more aggressive), `check_order`. `run_observed(.., &mut dyn Observer)` = same loop + `Snapshot` (positions,
open orders, last candles, counts) after each event; `run` = no observer, no
snapshot cost. Observer can't change run, must return fast. Rejected signal (risk, or venue refusing that one order:
`ExchangeError::is_order_rejection`) → `RunReport`, not fatal; other broker
errors (auth `-2015`, transport, clock, rate limit) stop run. Order ids deterministic (`o-1`, `o-2`, …).

## Strategy import — the Rust-specific problem
Python imports `.py` at runtime; Rust compiled, so "drop in strategy" has
four real answers. v1 registry; rest open via invariant 5.

| Approach | Recompile host? | User needs | Perf | Sandboxed | Use when |
|---|---|---|---|---|---|
| **Registry** (`inventory`) | yes | Rust + source tree | native | n/a | v1; strategies by you/contributors |
| **cdylib** (`libloading`) | no | Rust, exact compiler ver. | native | no (ABI/UB) | rarely |
| **Rhai script** | no | just write a script | interpreted | yes | external users, candle freq. |
| **WASM** (`wasmtime`) | no | compile to wasm | near-native | yes | external users, sandbox + speed |

v1: `inventory` so strategy file self-registers (`inventory::submit!`); add
one = add file — no central match — runs in backtest free.
`strategy.params` generic table each strategy deserializes into own typed
struct via serde (same compiled and later scripted). Rhai =
later "drop script" path; WASM = serious sandboxed/polyglot upgrade. cdylib
only if native speed AND binary distribution both needed (Rust no stable
ABI — compiler mismatch UB).

## Requirements → where they live
1. **Binance testnet + demo + live** — `BinanceSpotProvider` in `exchange::binance`.
   One client; base URL, keys from config (`exchange.network`:
   `testnet` default = `testnet.binance.vision`; `demo` = Demo Trading,
   `demo-api.binance.com` / `demo-stream.binance.com`, fake money, keys from
   demo.binance.com; `live`). Keys not interchangeable across networks. Same signing, same code path. More venues =
   new `Provider` impls, nothing else changes.
   Built: `BinanceSpotProvider` = `KlineStream` (combined ws streams; only
   `"x": true` closed bars emitted; dropped socket = error, no reconnect yet; no closed candle within
   `ws::stale_after` = 2×interval + 60s → `Feed` "stalled" error, so half-open
   socket can't leave bot running blind)
   + `BinanceBroker` (signed REST via `SignedClient`; key checked at startup
by signed `GET /api/v3/account` + `canTrade`, bad key = error before first candle; `Transport`/`Clock`
   traits for no-network tests). Orders carry run tag + our `OrderId`
   as `newClientOrderId` (`tb<start-ms base36>-o-N`: unique across runs);
   status/cancel by Binance `orderId` from submit ack, never client id; quote budget → `quoteOrderQty` (rounded toward zero,
   8 dp). `settle` polls each open/cancelling order's status + `myTrades`;
   order dropped only when terminal AND trades sum to `executedQty`.
   Books (`exchange::Books` = `Ledger + OrderJournal + HoldingRepository`,
   `SqlStore`): order journaled before submit, fills recorded by Binance trade
   id, position = stored holding → survives restart (engine seeds ctx
   positions from broker before `on_start`). Balances outside bot's own fills
   ignored. `run` db: `db.url`, default `sqlite://data/<network>.db`, migrated
   on open, network-stamped.
   Refusals identical to PaperBroker. Fee in third asset (BNB) → valued
   (`binance::fees`) at close of 1m `{asset}{quote}` candle containing trade,
   via that pair's `Instrument::notional`; approximation (myTrades omits
   Binance's rate). No such pair/candle → error, never guess.
   Reconcile (`binance::reconcile`, Binance = truth, books = which are ours):
   startup (in `provider.broker()`) and every exit incl. error and signal
   (`close_out`). SIGINT/SIGTERM/SIGHUP all = stop + close out (`cli::shutdown`);
   second signal abandons close-out (process never held hostage), next start
   reconciles. kill -9/crash: only next start; run under supervisor
   (`Restart=always`) so orders aren't unwatched long. Each unfinished journal order: find (venue id, else client
   id; `-2013` on pending = never placed), cancel if working (strategy state
   not persisted → leftover order has no owner), wait until terminal + trades
   = `executedQty`, record trades idempotently, close. Startup also refuses
   open `tb…` order not in books (bot under other db: fills untracked) and
   books holding > account free+locked. Others' open orders (manual, other
   software) only warned: books count own fills only; worst case bot order
   gets `-2010` (non-fatal rejection).
   `run --paper` = testnet or demo only; `run --live` needs `mode = "live"` +
   `exchange.network = "live"` + `--i-understand` (`cli::gate`, all combos
   tested); fake-money run refuses `*.binance.com` URL overrides except the
   two demo hosts.
   Rate limits (`exchange::rate_limit`, `binance::limits`): sliding-window
   log, not bucket (Binance windows fixed; full bucket = ~2× limit across a
   reset). Budget 80% of weight 6000/min, orders 100/10s, 200k/day; one
   limiter per provider shared by all its clients (weight per IP). Weights
   measured 2026-10-23 via `X-MBX-USED-WEIGHT-1M`; unknown endpoint = 20.
   Venue's higher usage header wins; 429/418 → hold all requests for
   `Retry-After` (default 60s), error still stops run. Request too big for
   budget → `RateLimited` (fatal).
   Every REST request bounded by `exchange.http_timeout_secs` (default 10,
   ≥ 1; `DEFAULT_HTTP_TIMEOUT` for clients built without config): stalled
   Binance → `Http` error → run stops, reconcile finds what a timed-out
   submit placed.
2. **Coin whitelist** — `config.whitelist: Vec<InstrumentId>` only universe
   bot trades. Plus `filters.min_24h_quote_volume` drops illiquid
   instruments: thin alts slip badly, may not fill — filter = safety
   feature. freqtrade pairlist idea — static list + volume filter for v1.
3. **SQLite now, MySQL later** — `storage` crate on **SeaORM** (backend-agnostic
   entities; switch `db.url` `sqlite://` → `mysql://`, no model changes).
   Repository traits wrap it; call sites never know backend. (sqlx lighter
   but per-dialect SQL; SeaORM chosen for MySQL path + migrations below.)
   **Money columns are exact-decimal TEXT (`varchar(40)`), never `DECIMAL`**:
   SeaORM on SQLite creates `DECIMAL` as `real(p,s)`, reads via `f64`
   (invariant 1). TEXT exact on both backends; cost: no SQL `SUM`/numeric
   `ORDER BY` on money — aggregate in Rust.
   Call sites see only `TradeRepository` (append-only fill log) and
   `HoldingRepository` (current `Position` per instrument, cache of log),
   both in core types; `SqlStore` implements. Entities crate-private.
   Live fills via `Ledger::record_fill(fill, instrument)`: one
   transaction appends trade + folds into holding via `Position::apply`
   (storage folds, caller can't pass drifted position); `FOR UPDATE` on MySQL.
   Reads re-validate through core constructors: corrupt row → error.
   Venue fills via `Ledger::record_venue_fill(fill, instrument, venue_trade_id)`:
   idempotent (unique `(instrument, venue_trade_id)`), so restart can replay
   venue trade history safely. `OrderJournal` (`orders` table): row written
   `pending` *before* submit, `open` + venue id on ack, `closed` after last
   fill recorded → crash leaves row to check. `SqlStore::open` applies pending
   migrations; `bind_network` stamps db with network (testnet/demo/live),
   mismatch = `WrongNetwork` error: one db per network.
4. **Backtest** — `backtest` crate: load cached klines, replay as `MarketEvent`
   stream through engine, emit report (realized P&L, max drawdown, trade
   count, win rate, Sharpe). Fill model stated below, not hidden.
   `fetch-data` pulls closed klines + instrument filters from keyless
   `data-api.binance.vision` (`exchange::binance::PublicClient`, native-tls)
   into `data/klines/<venue>/<SYMBOL>-<interval>.jsonl` and
   `data/instruments/<venue>/<SYMBOL>.json`; only missing head/tail ranges
   downloaded, then offline. Reads re-validate (corrupt cache → error).
   Filters are today's, not historical — approximation. `KlineFeed` merges
   instruments ordered by (close_time, id).
   `BacktestProvider` = cached feed + PaperBroker(NextOpenFillModel), through
   `engine::run`; `run_backtest` → `Report` (all `Decimal`). Report keeps own
   cash ledger (broker has none) replaying fills vs candles; equity = cash +
   holdings at latest close, once per close time. Max drawdown = fraction of
   running peak (starts at initial cash). `min_cash` = lowest cash after any
   fill; negative = overspent (broker can't refuse, report must show). Closed trade = sell; win = realized
   delta > 0. Sharpe = mean/sample-sd of per-bar equity returns × √(bars/yr,
   24/7: 8760 at 1h), rf 0; `None` if < 2 returns or zero sd.
   `cli backtest --slippage-bps` (and `make backtest SLIPPAGE_BPS=`) required,
   no default: market order never fills at price decision saw; silent 0 hides cost.
5. **User strategies** — implement `Strategy`, self-register via `inventory`.
   Config selects `strategy.name` + `strategy.params`. Trait
   provider-agnostic, so every user strategy runs in backtest free. Growth: import ladder above.
6. **Migrations** — SeaORM migration crate under `migration/`, run via
   `sea-orm-cli migrate up|down|fresh`. Every schema change = migration; never
   hand-edit live DB.
7. **freqtrade lessons applied** — config-driven; strategy decoupled from
   venue (via `Provider`); backtest/live parity core correctness property;
   event-driven design structurally prevents lookahead bias freqtrade only
   detects after fact with `lookahead-analysis`.

## Backtest fill model (assumptions are explicit)
Rules = `NextOpenFillModel` default (swap via `FillModel` trait in
Architecture).
Candle = four prices, can't tell whether high or low first. So:
- order only evaluated on candles opening after its `created_at`.
- market order → fills at **next** candle's open, taker.
- limit buy → first candle: open **<** limit (strict) → open, taker
  (marketable at submit). Else rests; fills at limit, maker, only if price
  trades **through** it (`candle.low < limit`). Touch at exactly limit = no
  fill (queue position unknown; order at last price just joins book).
  Later candles: never better than limit. Sell symmetric (`open > limit`,
  `high > limit`). IOC/FOK: first open or expire. No partial fills.
  Slippage on taker limit fill capped at limit — limit never fills worse.
- stop → triggers if `candle.high >= stop` (buy) / `candle.low <= stop` (sell);
  stop-market fills at stop, or open if gapped through, taker. Stop-limit
  never fills on trigger candle (path unknowable → worse case); limit from next.
- quote-budget market buy (`Size::Quote`, = Binance `quoteOrderQty`):
  broker sizes at slipped fill price — largest lot multiple with notional +
  quote fee ≤ budget. Spend never exceeds budget; too small → dropped.
- fee (taker/maker rate, received asset like Binance or quote) every fill;
  slippage (bps) taker fills only, rounded to tick against trader. Maker
  fills never slipped: limit can't fill worse than itself.
- PaperBroker refuses sells > holding − pending sells. No cash balance
  yet: buys not refused for lack of quote.
- **intra-bar ambiguity**: stop and target both in one candle →
  model can't know which first, assumes worse. Finer-timeframe
  replay = future. Backtest P&L = model output, never promise.

## Reference strategy
`ratio_hop` — coin-accumulation strategy from `ccxt/binance-trade-bot`: hold
coin strong vs peers, rotate `A → bridge → B` only when post-fee
ratio beats last-held amount of B, so unit count only ratchets up. Ships
**freefall guard the original lacks**: refuse rotating into coin down more
than `risk.freefall_pct` over last N candles, so reverse-greedy loop can't
accumulate coin collapsing to zero. Worked trait example, and
reminder: max units ≠ max value.
Params (`strategy.params`): `coins` (same bridge quote; first bought at
start with `quote_amount`), `fee` + `slippage_bps` (per-leg estimate;
slippage required, match backtest), `scout_margin`,
`freefall_pct` + `freefall_candles` (the `risk.freefall_pct` guard; lives in
params until config exists; `history_len` must exceed `freefall_candles`).
Evaluates once per close time when all coins' candles in. Est units of B =
lot(notional(lot(held A), sellpx A)·(1−fee) / buypx B)·(1−fee), sellpx =
close·(1−s) tick-down, buypx = close·(1+s) tick-up; rotate if
> baseline[B]·(1+margin), not in freefall, best ratio wins; leaving A sets
baseline[A] = units sold. Sell A, then on fill buy B with exact proceeds
(quote budget → never overspends). `order_type` required: `market` (taker,
next open + slippage; leg unfilled 2 evals → stops) or `limit` (GTC at last
close, maker, estimate without slippage; needs `limit_timeout`: stale buy
cancelled by id via `ctx.open_orders` + re-priced at new close, same budget;
stale sell cancelled → keep holding). Limit may never fill: real cost.
State (phase, baselines) not persisted. Start with sellable coin held (live
restart; engine seeds positions) → skip initial buy, hold it (most valuable
at last close if several), baselines restart from that eval's ratios. Flat
start (every backtest) unchanged. Restart between legs holds nothing → buys
afresh with `quote_amount`; earlier proceeds stay in bridge.

## Other built-ins
`buy_and_hold` — the benchmark every strategy is measured against: can't beat
it on same coin after costs → no edge. Params `coin`, `quote_amount`, `fee`,
`slippage_bps` (costs required, range-checked, unused). Decides once on coin's
first candle: flat → quote-budget market buy; already held (restart) → hold.
Never sells; rejected buy not retried. Keep minimal.
`sma_trend` — one coin, long or flat (cash = defensive; v1 spot no short).
Long while close > SMA(`sma_period` closes, current bar included) ×
(1 + `band_pct`); cash below SMA × (1 − `band_pct`). Buy = quote-budget market
(`quote_amount`), sell = lot-rounded full holding. "Cash" = holding not
sellable under venue filters (dust from base fee counts as cash, else never
rebuys). No decision while an order on coin is open. `fee`/`slippage_bps`
required, range-checked, unused in decisions. `history_len` ≥ `sma_period`.

`momentum_rotation` — multi-coin, one bridge quote (else stops at `on_start`).
Once per close time when all coins' candles in (ratio_hop guard), none with
open order: return over `lookback` bars per coin (needs `lookback + 1`
closes each, else nothing); best (tie → first listed) is target if return >
`min_momentum` (default 0), else cash. Sell every held coin ≠ target (sells
first), buy target if not held. Entry = fixed `quote_amount`, not sale
proceeds: losing hop overspends (backtest negative `min_cash`; live `-2010`,
stays cash). Bets winners persist; fails on sharp reversals, choppy
leadership. Dust = cash.

`donchian_breakout` — `sma_trend` skeleton. Channel = highest high / lowest low
of `channel_period` candles *before* current (needs `channel_period + 1`).
Flat and close > high → buy; long and close < low → sell; edge → nothing.
Excluding current bar = what makes rule satisfiable (close ≤ own high), not
lookahead (bar closed). Bets breakouts continue; whipsaws in ranges.

`dual_ma_cross` — `sma_trend` skeleton, two averages: flat and SMA(fast) >
SMA(slow) → buy; long and fast < slow → sell; equal → nothing. Level rule, not
cross event: first bar with `slow_period` closes buys at once if fast on top.
`1 ≤ fast_period < slow_period` checked at build. Fails in ranges (whipsaw,
lag).

`rsi_reversion` — one coin; the counterexample to the trend filters. Flat and
RSI < `oversold` → buy `quote_amount`; long and RSI > `overbought` → sell all.
RSI = v1 simple-average form (not Wilder) over last `rsi_period` changes
(needs `rsi_period + 1` closes): 100·G/(G+L); no losses 100, no gains 0, no
move 50. Bets on ranges; in sustained downtrend catches falling knife, holds
to the bottom (no stop, by design). Dust = cash; no decision while order open.

## Layout
```
Cargo.toml              workspace
crates/
  core/        (package `bot-core`: `core` clashes with Rust built-in) Price/Qty/Notional, ids, Instrument, Candle, MarketEvent, Signal, Order, Fill, Position
  strategy/    Strategy trait, Context, inventory registry, built-ins (buy_and_hold, donchian_breakout, dual_ma_cross,
               momentum_rotation, ratio_hop, rsi_reversion, sma_trend)
  exchange/    Provider + MarketData + Broker traits; binance{testnet,demo,live}; paper broker; binance ws
  engine/      shared event loop (backtest + live)
  backtest/    kline loader, BacktestProvider, runner, report
  storage/     SeaORM entities + repositories
  api/         read-only HTTP API (axum): /status /positions /open-orders /orders /trades;
               no write route ever (orders only from strategy via risk)
  cli/         binary: run | backtest | fetch-data | migrate
migration/     SeaORM migrations
config/        config.example.toml (committed); *.local.toml (gitignored). Env (BOT_*) overrides file.
data/          downloaded klines, per-network run dbs (gitignored)
deploy/        Prometheus + Alertmanager + Grafana compose, alerts.yml (+ promtool
               unit tests alerts_test.yml), dashboard; keep in sync with metric names
doc/           architecture.md (human-facing; update with design changes)
README.md      user-facing overview, quick start, config, limitations
```

## Commands
`make` lists targets (`check` = fmt-check + lint + doc + test, run before commit;
`migrate-*`, `fetch-data`, `backtest`, `run-paper`; vars `DATABASE_URL`,
`SYMBOL`, `FROM`, …). No live target on purpose: `--i-understand` must be
typed per run (invariant 7). Raw equivalents:
```
cargo run -p cli -- fetch-data --symbol BTCUSDT --interval 1h --from 2024-01-01   # cache history
cargo run -p cli -- backtest --strategy ratio_hop --from 2024-01-01 --to 2024-06-01
cargo run -p cli -- run --paper                  # paper vs testnet prices (default)
cargo run -p cli -- run --live --i-understand    # real orders, triple-gated
DATABASE_URL=sqlite://bot.db?mode=rwc sea-orm-cli migrate up   # apply migrations
cargo test && cargo clippy -- -D warnings && cargo fmt --check
```

## Configuration
Layered; each source overrides one above:
1. built-in defaults (code)
2. config file — `config/config.toml` (path override via `--config` or
   `BOT_CONFIG`); `config/config.example.toml` committed, documents every field
3. env vars, prefix `BOT_`, nest `__`
   (`BOT_EXCHANGE__NETWORK=demo`, `BOT_STRATEGY__NAME=ratio_hop`,
   `BOT_DB__URL=sqlite://bot.db`) — Docker/Heroku/CI path
4. CLI flags (`--paper`, `--live`, `--strategy`, …)

- Loader: `figment` — TOML provider + `Env::prefixed("BOT_").split("__")` —
  deserialized into one typed `Config` struct via serde with
  `#[serde(deny_unknown_fields)]` so typo errors, not silent default.
  (`config` crate alternative.)
- **Secrets are the exception to the file layer (invariant 6).** `api_key` /
  `api_secret` ONLY from fixed env vars (`BINANCE_API_KEY`,
  `BINANCE_API_SECRET`), never in TOML. File may name env var,
  never hold value. Missing in live mode → hard error at startup.
- Money fields TOML strings (`fee = "0.001"`); bare number errors
  (invariant 1). Env same: `BOT_X='"0.001"'` until loader coerces.
- Validate on load, fail fast: whitelist non-empty, fee/slippage ≥ 0, live mode
  requires `exchange.network = "live"` + `--i-understand` gate. Bad config
  never reaches trading loop.
- Real config = `config/config.toml` or `config/*.local.toml` (both
  gitignored) or env; only `config.example.toml` committed (and loaded by a
  test, so it can't rot). `exchange.network` defaults `testnet`. No field exists
  for a secret: `api_key` in a file = unknown-field error.

## Definition of done (every change)
Not finished until all hold — per task, not at end:
- Compiles; `cargo clippy -- -D warnings` and `cargo fmt --check` pass.
- **Tests ship in the same change.** Unit tests for new logic; regression test
  if touching backtester P&L path or fill model. No test = not done.
  Assert against *expected* values derived independently —
  never mirror whatever code returns (passes
  vacuously, locks bugs). Fill model and `ratio_hop`:
  owner supplies or confirms expected numbers.
- **Docs in the same change.** Public items (traits, structs, `pub fn`) carry `///`
  comments stating intent, invariants, not restating signature. Stale docs
  = bug: update with code.
- Change alters architecture, invariant, or config field → update
  CLAUDE.md and `config.example.toml` same change.

## Commits
- One commit = one logical change. Feature, fix, refactor, rename
  separate commits even same session — never bundled.
- Commit as you go, not at end. Unit complete (code + test
  + docs, per Definition of done) → commit before next. This,
  not size rule, keeps diffs small.
- Mechanical apart from behavioral: formatting-only or rename
  commit never changes logic, so review stays on logic.
- Each commit builds, passes tests alone (don't break `git bisect`).
- Conventional Commit subject, imperative, ≤72 chars: `feat(storage): add trade_stats`.
  Body says why, not what diff shows.
- Never commit secrets, `*.local.toml`, `data/`. Never `--force` shared branch.
- Don't commit unless asked or task clearly calls. Change spans
  several logical units → propose split first, not one big commit.

## Conventions
- Errors: `thiserror` in libraries, `anyhow` in binary. No `unwrap()` outside
  tests.
- Async `tokio`; JSON `serde`; money `rust_decimal`; logs `tracing` (structured,
  no money values at info level).
- Metrics via `metrics` facade (`engine::metrics`, `exchange::metrics` name
  consts; no recorder = no-op, backtest clean). Bounded labels only (no ids,
  amounts). Metric `f64` = display/alerting only, never read back into a
  decision or books. No money values from the facade.
  `run` exports via `[metrics]` (`cli::telemetry`): off by default, loopback
  unless `allow_public`, money gauges (position qty, cost basis, realized
  P&L) only with `include_money`; exporter failure = warning, never stops
  trading. `[api]` same rules: off, loopback default, non-loopback needs
  `allow_public` + token in `TRADEBOT_API_TOKEN` (not `BOT_*`: that prefix is
  config layer), live view via observer `watch` channel, bind failure =
  warning. API stays read-only; future controls only pause/cancel/stop.
- Every strategy and fill model get unit tests. Backtester gets
  known-input regression test so refactor can't silently change P&L.
- Honest claims: number not measured (backtest return, latency) →
  label unverified. Mirrors owner's first-principles preference.

## Crates
`tokio, async-trait, inventory, reqwest, tokio-tungstenite, serde/serde_json,
rust_decimal, sea-orm, sea-orm-migration, figment, thiserror, anyhow, tracing, clap, toml,
metrics, metrics-exporter-prometheus, metrics-process, axum`.
(`barter-rs` option if later prefer ready-made framework over
hand-rolled traits; `rhai`/`wasmtime` when strategy scripting needed.)