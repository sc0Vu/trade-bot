# CLAUDE.md

## Project
Rust trading bot. First venue Binance (crypto spot); multi-venue, multi-asset by design so equities follow without rewrite.
Event-driven, strategy-pluggable, backtester runs **same** strategy code as live. v1 scope: paper/backtest-correct on crypto spot; real
orders gated behind explicit config.

Scope ladder (build in order, don't foreclose later rungs):
**v1: crypto spot now → US equities next. v2: options & perpetual futures.**
Domain split so equities/options/perps *can* slot in.
**USDⓈ-M perpetuals in progress (owner, 2026-11-17)**, spot unchanged. Perps
break "owned inventory" — signed positions, funding payments (recurring cash
flow), mark-to-market unrealized PnL, broker-initiated liquidation.
`Position`-model change, not just new `AssetClass` variant — built bottom-up
(core → margin → paper/backtest → data → strategy API → live), each step
correct and tested before the next; no live perp orders until margin and
liquidation are modelled. Partial margin accounting that looks working is
worse than none. Plan + status: PROGRESS.md.

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
    fn market_price(&self, reference: Price, side: Side) -> Price;   // to tick against trader
    fn check_market(&self, q: Qty, reference: Price, side: Side) -> Result<(), FilterError>;
    // market order sends no price: check at market_price, never raw close (history
    // under today's filters is off tick → raw check called every holding unsellable)
    fn average_price(&self, history, now: Timestamp) -> Option<Price>; // band's avg, estimated:
    // Σquote/Σbase of candles closing in last avg_minutes (venue averages trades)
    fn check_band(&self, p: Price, side: Side, avg: Price) -> Result<(), FilterError>;
    // price_band: Option<PriceBand> = Binance PERCENT_PRICE_BY_SIDE (buy ∈ avg·[bid_down,
    // bid_up], sell ∈ avg·[ask_down, ask_up]); market-relative → not in check_order
    fn check_open_count(&self, open: usize, open_stops: usize, adding_stop: bool) -> Result<(), FilterError>;
    // max_open_orders / max_open_stops: Option<u32> = Binance MAX_NUM_ORDERS (stops
    // count too) / MAX_NUM_ALGO_ORDERS; counts = broker's book → brokers check at submit
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
`AssetClass::Perpetual` (linear USDⓈ-M; notional = qty·price like spot;
`allows_short` true): `Position::apply` → `apply_perp`: signed qty, basis =
signed entry notional (no fees: avg_cost = entry price, long or short), fill
against the position closes first (realized = closed·(px − entry)·sign), rest
flips at fill px; fees (quote only, base refused) and funding
(`apply_funding(rate, mark)`: −qty·mark·rate, longs pay at rate > 0) →
realized. `MarketEvent::Funding { instrument, rate, mark, ts }`.
`core::margin` (one-way USDⓈ-M): `Brackets` (contiguous from 0: floor/cap,
max_leverage, rate, cum; = `/fapi/v1/leverageBracket`), maintenance =
|notional|·rate − cum, `initial_margin` = notional/leverage,
`unrealized_pnl` = qty·mark − basis, `liquidation_price` = Binance FAQ
(WB − TMM₁ + UPNL₁ + cum − side·size·EP)/(size·MMR − side·size), bracket
chosen where the LP notional falls; isolated: WB = isolated wallet, others
0. Pinned by the FAQ's worked example (1,153.26 / 26,316.89).
`paper::PerpPaperBroker` (`PerpTerms { leverage, mode, brackets }` per
symbol, wallet in quote, assumed `clearance` fee rate): same FillModel /
slippage / fees (quote) as PaperBroker; fills move margin not cash
(isolated: opened·px/leverage wallet → position, closed share back,
realized + fees → wallet, funding → position margin; cross: shared
wallet, available = wallet + cross UPNL − cross IM − reservations).
Refuses leverage above the bracket at the resulting size and an order
whose exact wallet cost (`flow`: new margin − returned margin − realized −
fee; cross: ΔIM − realized) > available (submit at last close; fill at real price → dropped unfilled).
After each candle: low (long) / high (short) ≤/≥ liquidation price →
closed at LP (tick against trader) + clearance fee capped at margin left
there (isolated: its margin, cross: wallet), id `liq-N`; never loses past it.
Backtest: `BacktestProvider::with_perp(PerpSetup { terms, clearance })`
→ PerpPaperBroker (wallet = initial cash; asset classes = Perpetual only),
`with_events` merges `Funding` (engine: Funding → ctx position refreshed,
no fill). `Report::compute_full(.., events, ..)`: perp fills move cash by
realized only, funding booked before fills settled at the first close ≥ it
(broker order; Binance stamps funding ms after the hour), equity = cash + qty·close
− basis, closed trade = fill against the position; `Report.funding`.
Mark price: `MarketEvent::Mark(Candle)` (mark-price bar, before the trade bar
of the same close; strategies ignore it; ts = close). PerpPaperBroker checks
liquidation on the mark bar of the same open time if delivered, else the
trade bar. Backtest: `fetch-data` caches `/fapi/v1/markPriceKlines` under
`mark/<venue>/<SYM>-<iv>.jsonl` (`fetch_marks`), CLI replays them
(`mark_events`; none → warning). Paper: `usdm_paper::MarkWatch` fetches each
closed bar's mark bar over REST (published at once, checked 2026-11-18; miss
= warning, trade bar). Live: the venue liquidates itself.
Data (`exchange::binance::usdm`): venue `binance-usdm` (own ids, cache,
books); `PublicClient::usdm(host)` klines `/fapi/v1/klines`;
`perp_instrument` from `/fapi/v1/exchangeInfo` (PERPETUAL + margin =
quote only; MIN_NOTIONAL `notional`, caps `limit`, no band: futures'
PERCENT_PRICE is vs mark) + `liquidationFee`; `funding_rates` (mark
`None` when venue left it empty: pre-2023 history); `brackets` (signed
`/fapi/v1/leverageBracket`, numbers read from text via
`SignedClient::signed_text`; demo key works on demo + testnet hosts).
Cache (`backtest::perp`): `funding/<venue>/<SYM>.jsonl`, `perp/<venue>/<SYM>.json`
(`PerpData { liquidation_fee, brackets: Option }`); `fetch_funding` head/tail
like klines; `funding_events`: missing mark → bar open containing it (or
last close after), none before → skipped, count noted. CLI: `Venue::
BinanceUsdm` (`binance-usdm`): `run` local paper or venue orders (`[perp] fills`);
`fetch-data` + funding, fee, brackets (`--brackets-from demo|testnet|live`,
key from env, failure keeps cached); `backtest --leverage N --margin
isolated|cross [--clearance r]` (default = max cached liquidationFee), fees
in quote, no brackets → refused, `--earn-ratio` refused. Checked
2026-11-17: 1x BTC Feb–Aug 2026 = hand recompute exactly (802.23049863).
Paper run (`binance::usdm_paper`): `UsdmPaperProvider` = `FundingWatch`(
`ReconnectingFeed`(ws `wss://fstream.binance.com/market/stream` — legacy
`/stream` upgrades but sends no klines, 2026-11-17 — + `/fapi/v1/klines`)) + `PerpPaperBroker`
(today's filters, clearance = config else max liquidationFee). `FundingWatch`
polls `fundingRate` after last seen (start = run start, nothing replayed)
every `FUNDING_POLL` (5 min), delivers before the next event stamped with
its ts; no mark → skipped + warn; failure = warning. Config `[perp]
{ leverage, margin, brackets_from (default demo), clearance }` + `[paper]`
required, no 1s, no URL overrides; brackets signed via env key at start;
no db / API / resume (flat broker), like MAX paper.
Orders (`binance::usdm_live`, `[perp] fills = "venue"`; default "local" =
paper above): `UsdmProvider` on `exchange.network`'s futures host (live
fapi / demo-fapi / testnet.binancefuture.com; ws `/market` path on each;
testnet and demo = same book), gated like spot (`--paper` = testnet/demo,
real money = all three locks). Startup: `prepare` (one-way mode required,
never switched; marginType, `-4046` ok; leverage, Binance's echo must
match), `reconcile` (journal rows: find, cancel, wait, record, close;
`tb…` orphans refused / `--cancel-orphans`; `align` over 7 days). `UsdmBroker`:
`POST /fapi/v1/order` MARKET/LIMIT; stops → `POST /fapi/v1/algoOrder`
(CONDITIONAL, triggerPrice, CONTRACT_PRICE; journal id = algoId; polled as
algo until `actualOrderId`, then as that order; reconcile + orphan scan check
algo orders first / `openAlgoOrders`; checked on demo 2026-11-18),
`Size::Quote` → base at last close (paper converts at fill: small diff);
settle polls every working order (`order` + `userTrades?orderId`); Funding
event → `/fapi/v1/income` FUNDING_FEE since last read (first: 7 days back if
held, else run start) → `Ledger::record_funding` (table `funding`, migration
6, unique (instrument, tranId), folded into holding realized via
`Position::credit`); each Candle `align`:
`/fapi/v3/positionRisk` vs books, differ → `userTrades` since run start, trades
not from a working order of ours adopted (`venue-<orderId>`, warn:
liquidation/ADL/manual — bot needs its symbols to itself), still differ with
nothing working → run stops. BNB fee → error (turn off fee burn). Futures
rejections `-2019 -2027 -2028 -4164` = order rejections. Books: same schema
(signed holding), own db `data/<network>-usdm.db`; exit cancels orders,
positions stay open. Strategy state + `[api]` as spot (`cli::observe_run`, shared). Checked on demo 2026-11-17
(sma_long_short 1m, 2x isolated): short, flip, stop → books = Binance
(position 0.0031, 3 trades, realized −0.59727811), no orders left.
Spot positions non-negative owned inventory. Quantity sign = value
concern, not compile-time invariant — v2 signed (short/perp) position
must reuse type, not force rewrite. Don't encode `quantity ≥ 0` as
type-level law (no unsigned/`NonZero` qty, no `try_from` rejecting negatives).
Sign = runtime policy: `Instrument::allows_short()` (true for Perpetual only),
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
                   CorporateAction(CorpAction),
                   Delisting { instrument, at: Option<Timestamp>, ts },
                   TradingStatus { instrument, trading: bool, ts } /*, Expiry deferred */ }
// TradingStatus: venue halt/resume, no schedule (ctx.halted(id); ctx.tradable
// = !delisted && !halted). While halted: risk + PaperBroker refuse every order
// on it (resting kept), engine exit pauses, multi-coin strategies don't wait
// for it (completes), don't trade it (portfolio), don't pick it (rankings),
// ratio_hop holding it waits, grid idles. Live-only (backtest gaps → cut).
// Delisting: venue scheduled `instrument` to stop trading at `at` (None =
// schedule withdrawn); ctx.delisting(id) / ctx.delisted(id) (now ≥ at). From
// announcement risk refuses orders that could open/add (buy unless short;
// sell when flat/short on a shortable class: spot = no buys); from at − EngineConfig.delist_exit
// (`[engine] delist_exit_hours`, default 24) engine::exit_delisting each event
// cancels working orders on it (once each), then once none working/cancelling
// closes the lot-rounded position at market, reduce-only (sell long, buy short; dust stays); engine-own orders: no
// on_order/on_rejected, refusals → RunReport. Same in backtest (parity).
// CorpAction is empty enum in v1 → variant unconstructible until equities.
// MarketEvent::ts(): candle → close_time (bar known), never open_time.
// Backfill(Candle): late bar (live: warm-up / reconnect gap; backtest: warm-up
// before --from) → engine adds to history, default on_event ignores (not a
// decision), paper broker doesn't settle on it. Only deliberate live/backtest
// difference: backtest has no outages.

trait Strategy: Send {
    fn name(&self) -> &str;
    fn on_start(&mut self, _ctx: &mut Context) {}
    // default: Candle → on_candle, other events ignored
    fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal> { .. }
    fn on_candle(&mut self, _ctx: &mut Context, _c: &Candle) -> Vec<Signal> { vec![] }
    fn on_fill(&mut self, _ctx: &mut Context, _f: &Fill) {}  // signals via ctx.submit
    fn on_order(&mut self, _ctx: &mut Context, _o: &Order) {}  // accepted: learn its id
    fn on_rejected(&mut self, _ctx: &mut Context, _s: &Signal, _reason: &str) {}
    // both at routing, end of the emitting event: before any fill of it (an
    // order can fill before it's ever in open_orders); hook signals → next event
}
// Context: now, instrument(s) (universe = resolved whitelist), position,
// open_orders (ids for Signal::Cancel; as of last settle), cancelling_orders
// (cancelled, venue hasn't confirmed: may still fill; never cancel again),
// history (≤ history_len closed candles, oldest first, ends at current event),
// delisting(id), delisted(id), halted(id), tradable(id), params, submit. Engine mutates via `strategy::driver` (strategy calling it
// only corrupts own view; broker = source of truth).

#[async_trait] trait MarketData { async fn next(&mut self) -> Result<Option<MarketEvent>>; } // feed error stops run
#[async_trait] trait Broker {
    async fn submit(&mut self, o: Order) -> Result<OrderId>;
    async fn cancel(&mut self, id: OrderId) -> Result<()>;
    async fn settle(&mut self, e: &MarketEvent) -> Result<Vec<Fill>>; // pending vs NEW event; live: drain venue fills
    fn position(&self, i: &InstrumentId) -> Position;
    fn open_orders(&self) -> Vec<Order>;  // snapshot → ctx after each settle
    fn cancelling_orders(&self) -> Vec<Order> { vec![] } // cancelled, may still fill → ctx
                                          // (Binance: until closed + all trades in)
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
broker, `created_at` = event ts → `on_order` / `on_rejected`; (6) delisting exit.
`EngineConfig { .. }` literals end `..Default::default()` (new fields break no caller).
`Signal::Reduce { instrument, side, qty, kind }` = reduce-only (venue
`reduceOnly`): risk as Submit + `Order::with_reduce_only` (base size only;
`Order.reduce_only()`, serde field only when true), never refused as opening
on a delisting; every broker `check_reduce_only` (qty ≤ the position it
faces, else refused; spot: buys refused), perp paper clips the fill to
what's left (never opens/flips), live sends `reduceOnly=true`; Rhai
`market_reduce(id, side, qty)`.
Risk v1: whitelist, only reducing orders on a delisting instrument, sizing (quote → base
at order price, except market buy keeps quote budget; market reference =
last close rounded to tick against trader, buy up / sell down: market order
sends no price, history under today's filters can be off tick), lot/tick
rounding (never more aggressive), `check_order`, then limit price
(limit, stop-limit) vs `price_band` around `average_price(history, now)`
(Binance `-1013 PERCENT_PRICE_BY_SIDE` caught before submit, same in
backtest; no band / no avg → unchecked). Strategy state: `save_state() -> Option<Value>` (versioned, money as strings)
in `Snapshot.strategy_state` after each event; `run_observed(.., resume)`
calls `restore_state(state, &ctx)` after `on_start`, all-or-nothing, must
refuse state contradicting books; refused → fresh + warning.
`Broker::take_recovered_fills` (startup reconcile) → engine delivers to
`on_fill` with first non-Backfill event (history warm).
`run_observed(.., &mut dyn Observer)` = same loop + `Snapshot` (positions,
open orders, last candles, counts) after each event; `run` = no observer, no
snapshot cost. Observer can't change run, must return fast. Rejected signal (risk, or venue refusing that one order:
`ExchangeError::is_order_rejection`) → `RunReport`, not fatal; other broker
errors (auth `-2015`, transport, clock, rate limit) stop run. `Strategy::fault() -> Option<String>`
(default `None`; script runtime error / budget) checked after each hook, before routing →
`EngineError::StrategyFault`, nothing it queued routed. Order ids deterministic (`o-1`, `o-2`, …).

## Strategy import — the Rust-specific problem
Python imports `.py` at runtime; Rust compiled, so "drop in strategy" has
four real answers. v1 registry; rest open via invariant 5.

| Approach | Recompile host? | User needs | Perf | Sandboxed | Use when |
|---|---|---|---|---|---|
| **Registry** (`inventory`) | yes | Rust + source tree | native | n/a | v1; strategies by you/contributors |
| **cdylib** (`libloading`) | no | Rust, exact compiler ver. | native | no (ABI/UB) | rarely |
| **Rhai script** | no | just write a script | interpreted | yes | external users, candle freq. |
| **WASM** (`wasmtime`) | no | compile to wasm | near-native | yes | external users, sandbox + speed |

Built: **Rhai** (`strategy::script`, feature `rhai`, off by default):
`strategy.name = "rhai"`, `params.script` = path (cwd-relative), other params
= script's own (TOML float refused). Rhai `no_float` + `decimal`: every
non-integer number = `Decimal` (invariant 1); `sync`, `no_time`; no eval /
import (dummy resolver) / file / net; `MAX_OPERATIONS` 1M per hook call, call
depth 64, size caps; `print` → log. Hooks optional, named like trait's
(`on_start/on_candle/on_fill/on_order/on_rejected(ctx, ..)` → `()` | signal |
array), `check_params(params)` at build (throw = refuse), `restore(saved,
ctx)` → `()` take | string refuse (no `restore` → always fresh). `this` = one
state map, encoded to JSON after every hook (decimal = `{"$decimal": s}`;
unsavable value = fault in backtest too), saved in envelope with script
SHA-256 (edited script → fresh). `ctx` = Arc snapshot of `Context` per hook;
candles shared via an `Arc` mirror synced incrementally (rebuilt if ends
disagree); `ctx.history(id, n)` = newest n (whole history costs O(history_len)
per call). Release, 265k 1m bars: native 2.9 s, script 11.8 s (~35 µs/bar).
Any script error → `Strategy::fault` → run stops. Shared boundary in
`script/mod.rs` (source, params split, envelope) for Lua/WASM later.
Reference port `strategies/sma_trend.rhai` = built-in's fills exactly
(`backtest/tests/rhai_script.rs`, 2000-bar walk, 3 param sets) + restart
case. `make check` runs `FEATURES` (`strategy/rhai,backtest/rhai,cli/rhai`);
`cli` feature `rhai` on in release binaries + container (no-recompile users).
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
   `"x": true` closed bars emitted; dropped/stalled socket → `feed::ReconnectingFeed` reconnects (backoff
   1s→60s, ≤ `exchange.max_outage_secs` 600, then error stops run), subscribe
   then backfill gap over REST (all but newest per coin = `Backfill`, newest =
   `Candle`), dedup by last close time; backfill cut at oldest coin's newest
   bar (coins fetched in turn; later extra bar would run time backwards),
   rest left to stream (warm-up fetches 60 extra); first connect warms up `history_len`
   bars as `Backfill`; no closed candle within
   `ws::stale_after` = 2×interval + 60s → `Feed` "stalled" error, so half-open
   socket can't leave bot running blind)
   + `BinanceBroker` (signed REST via `SignedClient`; key checked at startup
by signed `GET /api/v3/account` + `canTrade`, bad key = error before first candle; `Transport`/`Clock`
   traits for no-network tests). Orders carry run tag + our `OrderId`
   as `newClientOrderId` (`tb<start-ms base36>-o-N`: unique across runs);
   status/cancel by Binance `orderId` from submit ack, never client id; quote budget → `quoteOrderQty` (rounded toward zero,
   8 dp). `settle` polls open/cancelling orders' status + `myTrades`;
   order dropped only when terminal AND trades sum to `executedQty`.
   Which: `binance::user_stream` (WebSocket API
   `userDataStream.subscribe.signature`, HMAC ok; listenKey = HTTP 410)
   names orders with an `executionReport` = hint only, fills still REST.
   Polled: named, submitted/cancelled since last settle, trades lagged
   status last poll; ALL before first subscribe, after reconnect, while
   down, after unreadable message, every `FULL_POLL_EVERY` (60s). Never
   connects → all every event (old behaviour); failure = warning, retry
   1s→300s, 60s silence = dead. No listenKey/keepalive: subscription
   lives with connection; Binance cuts at 24h + sends `serverShutdown` →
   renewed at 23h (`Timing::max_age`) or on that event, no backoff. `exchange.user_stream` (default true);
   off while `rest_url` overridden (would watch real host).
   Books (`exchange::Books` = `Ledger + OrderJournal + HoldingRepository`,
   `SqlStore`): order journaled before submit, fills recorded by Binance trade
   id, position = stored holding → survives restart (engine seeds ctx
   positions from broker before `on_start`). Balances outside bot's own fills
   ignored. `run` db: `db.url`, default `sqlite://data/<network>.db`, migrated
   on open, network-stamped.
   Refusals identical to PaperBroker. Fee in third asset (BNB) → valued
   (`binance::fees`) at `close` of 1m `{asset}{quote}` kline containing trade
   (REST includes minute in progress: valued during it = latest price then,
   after = final close), via that pair's `Instrument::notional`; approximation (myTrades omits
   Binance's rate). Minute not yet published (REST lags ~5 s on demo) →
   retried every 2 s, ≤ 30 asks; no such pair/candle → error, never guess.
   Reconcile (`binance::reconcile`, Binance = truth, books = which are ours):
   startup (in `provider.broker()`) and every exit incl. error and signal
   (`close_out`). SIGINT/SIGTERM/SIGHUP all = stop + close out (`cli::shutdown`);
   second signal abandons close-out (process never held hostage), next start
   reconciles. kill -9/crash: only next start; run under supervisor
   (`Restart=always`) so orders aren't unwatched long. Each unfinished journal order: find (venue id, else client
   id; `-2013` on pending = never placed), cancel if working (strategy state
   not persisted → leftover order has no owner), wait until terminal + trades
   = `executedQty`, record trades idempotently, close. Startup-recovered fills
   (`Reconciled.fills`) → `BinanceBroker::with_recovered` →
   `take_recovered_fills` → strategy `on_fill`. Startup also refuses
   open `tb…` order not in books (bot under other db: fills untracked;
   `Orphans::Refuse` default, `run --cancel-orphans` = `Orphans::Cancel`:
   cancel them + start, never adopted (partial history), their fills stay
   outside books; venue can't tell lost db from second live instance) and
   books holding > account free+locked. Others' open orders (manual, other
   software) only warned: books count own fills only; worst case bot order
   gets `-2010` (non-fatal rejection).
   `run --paper` = testnet or demo only; `run --live` needs `mode = "live"` +
   `exchange.network = "live"` + `--i-understand` (`cli::gate`, all combos
   tested); fake-money run refuses `*.binance.com` URL overrides except the
   two demo hosts.
   Pair status (`binance::status`): every network; `StatusWatch` wraps the
   feed (inside `DelistWatch`), polls `exchangeInfo?symbols=[..]` (weight 20)
   before first event + every `STATUS_POLL` (60 s); status ≠ `TRADING` (HALT,
   BREAK, ..) → `TradingStatus { trading: false }`, back → `true`; changes
   only, stamped like delistings; `-1121` (unknown symbol, e.g. delisted
   since) → per-symbol asks, unknown = not trading (public client reports
   venue errors as `Http` with body: feed keeps retrying 429). Checked on
   demo 2026-11-17 (`live_trading_status`, ignored test).
   Simple Earn (`binance::earn`, config `[earn]`: enabled=false, ratio, band
   "0.01", product flexible|locked + locked_days, interval_minutes 60; live
   network + Binance only, validated): `EarnSweep` from `BinanceBroker::settle`
   when due and no buy of ours working; reads `/api/v3/account` free quote +
   flexible (and locked) positions fresh each sweep (5xx = unknown outcome →
   seen next time, never retried); `plan`: fund = spot free + earn, move to
   ratio·fund if off by > band·fund (subscribe ≥ product min, ≤ spot; redeem
   flexible only; 8 dp truncated). Locked: top up only, `redeemTo` SPOT, no
   auto-renew. Ratio counts the account's whole quote cash. Amounts at debug
   log only. Docs read 2026-11-17 (list/position weight 150, sub/redeem 1);
   demo/testnet 404; not yet run live.
   Delisting schedule (`binance::delist`): live network on Binance's own host
   only (demo/testnet 404, checked 2026-11-17) → `DelistWatch` wraps the feed,
   polls `GET /sapi/v1/spot/delist-schedule` (`SignedClient::keyed`: key, no
   signature; weight 100 per docs) before first event + every `DELIST_POLL`
   (1h); whitelisted coin newly scheduled / moved → `Delisting { at }`, off
   the list before its time → `at: None`; stamped with next market event's
   ts (never wall clock: backfill after reconnect is older); poll failure =
   warning. Not yet exercised against the live host (no live key here).
   Rate limits (`exchange::rate_limit`, `binance::limits`): sliding-window
   log, not bucket (Binance windows fixed; full bucket = ~2× limit across a
   reset). Budget 80% of weight 6000/min, orders 100/10s, 200k/day; one
   limiter per provider shared by all its clients (weight per IP). Weights
   measured 2026-10-23 via `X-MBX-USED-WEIGHT-1M`; unknown endpoint = 20.
   Limits replaced by `exchangeInfo` `rateLimits` on every instrument lookup
   (`limits::apply_published`, no extra weight); defaults = fallback.
   Venue's higher usage header wins; 429/418 → hold all requests for
   `Retry-After` (default 60s), error still stops run. Request too big for
   budget → `RateLimited` (fatal).
   Every REST request bounded by `exchange.http_timeout_secs` (default 10,
   ≥ 1; `DEFAULT_HTTP_TIMEOUT` for clients built without config): stalled
   Binance → `Http` error → run stops, reconcile finds what a timed-out
   submit placed.
1b. **MAX (MaiCoin) spot** — `exchange::max`. NO testnet/sandbox: one
   production host, real balances → fake-money MAX = live MAX data + local
   PaperBroker, never orders. Built: `MaxPublicClient` (keyless):
   `/api/v3/markets` → `Instrument` (`max:BTCUSDT` = market `btcusdt`).
   Precision per MARKET not coin (BTC 8 dp on btctwd, 6 on btcusdt): lot =
   10^-`base_unit_precision`, tick = 10^-`quote_unit_precision`, may be
   negative (-3 → 1000) → `max::public::step`. Minimums + kline prices are
   bare JSON numbers → read as raw text (`serde_json` `raw_value`) →
   exact `Decimal` (invariant 1), never f64. REST klines `/api/v3/k`
   (`[open s, o, h, l, c, v]`, ≤ 10,000/page, no 1s) carry NO quote volume →
   estimated v × (o+h+l+c)/4 (avwap on MAX approximate). No band/caps
   published. Limits 1200 req/min (`max::default_limits`, 80%).
   `fetch-data` / `backtest` `--venue binance|max` (`backtest::HistorySource`
   per venue); cache under `data/{klines,instruments}/max/`.
   Live feed (`max::ws`): `MaxKlineStream` on `wss://max-stream.maicoin.com/ws`
   (`kline` channel, closed = `"x": true`, empty bars still close, `qv`
   exact; missing → estimated), CLIENT must ping (MAX drops after 130 s
   silent → ping every 30 s), stall = `binance::ws::stale_after`.
   `MaxWsConnector` + `MaxRestKlines` plug into `ReconnectingFeed` (same
   warm-up/backfill/reconnect rules). Ignored live test:
   `cargo test -p exchange live_feed -- --ignored`.
   `MaxPaperProvider` (`max::paper`) = MAX feed + PaperBroker
   (NextOpenFillModel, `fees`/`slippage`/`initial_cash` in first
   instrument's quote): paper on MAX = backtest model on live bars; no key,
   in-memory books (every run starts flat). Gated by `max::status::
   MarketStatus` (REST statuses, then public `market_status` channel;
   cancel-only → no submit, suspended → no submit/cancel, undocumented → no
   submit; channel down → last known kept, reconnect 1s→60s; status and
   filter changes logged, filters apply on restart; REST and ws filters
   agree, checked live 2026-10-28).
   Config `exchange.venue = binance|max` (default binance; whitelist must
   match venue). MAX: `[paper]` {initial_cash, fee, slippage_bps — strings,
   slippage required} required, no 1s, no rest/ws_url, `mode = "live"` /
   `--live` refused (`gate::decide_max`); `exchange.network` ignored.
   `cli::run_max_paper`: no creds, no db, no API (warned), no state resume
   (would contradict a flat broker), no close-out (nothing on MAX).
1c. **eToro** — `exchange::etoro` (docs api-portal.etoro.com, read
   2026-11-18; OpenAPI v1.417.0). A broker: positions, not an order book.
   Keys `ETORO_PUBLIC_KEY` (x-api-key) + `ETORO_PRIVATE_KEY` (x-user-key),
   fresh `x-request-id` UUID (= order idempotency key), non-default
   User-Agent (Cloudflare 1010 otherwise); one user key = one account
   (demo key → `403 InsufficientPermissions` on real paths); demo paths
   splice `/demo/` (`Account::path`). Limits per user key /60 s: market data
   120, execution 20, info 60 (80% budgeted, 429 → `Retry-After`).
   Instruments `etoro:<symbol>` by exact `symbol` (`/api/v2/market-data/
   instruments?symbols=`; search is prefix) + eligibility (`/api/v2/
   trading/info[/demo]/eligibility`): only the real asset (settlement
   real, long, 1x) → `AssetClass::Spot`, quote USD; no tick/unit step
   published → tick 1e-8, lot 1e-6 fractional (observed: every position eToro
   opened had 6 decimals; 1 whole), min notional =
   `minPositionExposure`. History `/api/v1/data/instruments/{id}/candles`
   (date range, exact strings, bid side, loaded daily); live-current
   `/api/v1/market-data/.../history/candles/desc/{iv}/{n}` (numbers read
   raw, forming bar dropped). Volume often null → 0, quote volume 0 (no
   avwap). CLI `fetch-data`/`backtest --venue etoro` (fees in quote; gap
   cut default 120 h: markets close; warm-up = last N bars, not N×interval).
   Run (`etoro::{broker,feed}`): `run --paper` = demo account (network
   testnet/demo), real = live gate's three locks; books
   `data/<network>-etoro.db`, state + API as spot. `EtoroFeed` polls
   (warm-up `recent(history_len)` as Backfill; then `LAG` 5 s after each
   boundary, newest 5 closed bars, new ones as Candle; closed market = no
   bar, not a stall; late bar older than delivered = dropped; failing >
   max_outage = error). `EtoroBroker` (via `EtoroApi`, testable): accepts
   what `TouchFillModel::accepts` does (backtest = live); buy = open
   position (real, 1x), market, limit ≥ last close → market (an `mit`
   above the market waits for a rise, like a buy-stop: demo 2026-11-18),
   limit below last close → `mit` with `triggerRate`; a triggered `mit`
   runs as a NEW order: its own lookup stays 12 forever, it leaves the pnl
   `orders` list, and the execution order's lookup answers with the mit's id
   as `orderId` (demo 2026-11-18) → `triggered_execution` looks up each
   unseen position's `orderID` on the instrument (settle and reconcile;
   5 settles without a match → ended unfilled)
   (waits as status 12; cancel = `DELETE /api/v2/trading/execution[/demo]/
   orders/{id}` → 7), by amount (quote budget, market only; eToro's
   fee on top: budget overshoot ≤ fee) or units, poll `orders:lookup` by
   orderId (lookup by request id = 404 on demo) → fill per
   `positionExecutions` (units, avgPrice, fees); sell = close the account's
   real positions oldest first (whole when selling all; else
   `UnitsToDeduct`; eToro refuses < ~$10 deducted, error 776, async),
   poll `close-orders/{id}` → fill per closed position at `rate`, fee =
   units·rate − proceeds (cent-rounded), split by gross share; terminal
   unfilled = order dropped (warn). Limit sell above last close = take-
   profit (`PATCH /api/v2/trading[/demo]/positions/{id}` `takeProfitRate`,
   202 async) on the oldest free positions whose units sum exactly to the
   qty (else refused); at/below last close = market close. TP legs: one
   pnl read per settle; confirmed by stream `takeProfitUpdated` or the
   position's rate; refused (stream error, e.g. 1053 below a long's market)
   → dropped; position gone → fill from stream close (exact fee) else trade
   history (`/api/v1/trading/info/trade[/demo]/history`, fee unknown → 0,
   warn). Positions under our TP are excluded from market sells. Cancel:
   mit → DELETE; TP → PATCH `clearTakeProfit` (cancelling until the rate is
   gone; a fill that won the race is reported); market → refused. Market
   close fee = stream `closingData.fees` when it saw that close order, else
   derived. Stream (`etoro::stream`, started with the broker):
   `wss://ws.etoro.com/ws`, Authenticate {userKey, apiKey} (+ User-Agent),
   Subscribe `private:demo` | `private`; pushes `OrderEvent` (undocumented
   name) with `positionsExecutions[].closingData` {orderId, units,
   avgPrice, fees} and position `takeProfitUpdated` {takeProfitRate |
   errorCode}; heartbeat ~30 s; 90 s silence / drop → reconnect 1→60 s.
   Market data topics: `instrument:<id>` = live bid/ask (~1/s, strings,
   partial updates; snapshot has `IsMarketOpen`); `candle:<id>` refused as
   invalid (2026-11-18), so bars stay on REST (quotes would differ from
   the bid-side history candles). REST transport failures retried ×3
   (0.5 s, 1 s) with the same `x-request-id` (eToro's idempotency key);
   HTTP answers never retried. Journal: each venue order a row (`<tag>-
   o-N`, closes `.c<k>`, take-profits `.t<k>` with id = position), fills by
   `open-<order>-<pos>` / `close-<order>-<pos>`. Reconcile (start + exit):
   rows without id closed (can't look up); opens waiting (11, 12) cancelled
   then polled to the end; TP rows cleared if the position is open, else
   recorded from history; books ≠ account real units → refuse start
   (more: closed outside; less: opened outside / split). During the run
   (`with_account_check`, on in `EtoroProvider`): each new close time,
   nothing working, no fill this settle → pnl read, any instrument's books
   ≠ account units → run stops (`account_matches`; one Info call per close
   time).
   Market sell plan (`close_plan`, checked before any leg is sent): units
   rounded down to the step; a partial close leaving < $1 (eToro: "remaining
   live position amount … < 1") closes the whole position; one deducting <
   `min_notional` (eToro error 776, MinPositionAmount) refused locally (a
   rejection the strategy handles, not an async refusal it retries).
   Precision audit 2026-11-18: Binance spot/futures LOT_SIZE + PRICE_FILTER
   published (MARKET_LOT_SIZE not checked locally: oversize = -1013
   rejection); MAX per-market precision published; eToro none (unit 1e-6
   observed, price 1e-8 assumed).
   History NOT split-adjusted, and mixes scales near splits (checked
   2026-11-18: NVDA 2024-06-06 1205 → 121; AAPL 2019-2020 single bars at
   1/4 price; NVDA.RTH 2024-06-06): backtests across a split are wrong;
   live books don't follow a split (corporate actions unmodelled): the
   account check stops the run at the first idle bar after one. Examples avoid split windows (AMZN.RTH, not NVDA.RTH).
   `limitIOC` not used: on demo a buy limited 3% below the ask filled at the
   ask (2026-11-18). Stock orders cost $1 each on demo. Demo 2026-11-18: hand
   round trip + sma_trend 1m run (buy, sell, stop) → books = account to the
   cent (realized −0.41540728 vs credit −0.42).
   USD-priced only: `instrument` refuses conversionRateAsk/Bid ≠ 1
   (`/api/v1/market-data/instruments/rates`; BARC.L pence 0.013219, 7203.T
   yen). Market hours: `feed::MarketHours` wraps the feed, polls each
   instrument's `isCurrentlyTradable` (`/api/v1/market-data/search`;
   `isExchangeOpen` ≠ open: false for BTC) before the first event + every
   `STATUS_POLL` (60 s) → `TradingStatus` on change (closed = halted: risk
   refuses, single-coin waits, multi-coin trades the open ones), stamped
   with the next event; poll failure keeps last state. `AAPL` = extended
   hours, `AAPL.RTH` = regular hours. Config `[etoro] w8ben_signed`
   (default false; etoro venue only): real account + eligibility
   `requiresW8Ben` + not declared → refused at start (demo exempt).
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
   Perpetual funding: `Ledger::record_funding(instrument, venue_id, amount,
   ts)` — `funding` table (migration 6), idempotent per (instrument,
   venue id), folds into the holding's realized P&L in the same txn.
   Call sites see only `TradeRepository` (append-only fill log) and
   `HoldingRepository` (current `Position` per instrument, cache of log),
   both in core types; `SqlStore` implements. Entities crate-private.
   Live fills via `Ledger::record_fill(fill, instrument)`: one
   transaction appends trade + folds into holding via `Position::apply`
   (storage folds, caller can't pass drifted position); `FOR UPDATE` on MySQL.
   Reads re-validate through core constructors: corrupt row → error.
   Venue fills via `Ledger::record_venue_fill(fill, instrument, venue_trade_id,
   client_order_id)`: idempotent (unique `(instrument, venue_trade_id)`), so
   restart can replay venue trade history safely. `trades.client_order_id`
   (migration 5) = journal's run-tagged id (`Fill.order_id` restarts `o-1`
   each run; reconcile-recorded fills carry client id there instead); NULL
   for paper and pre-migration rows (no exact link → not guessed).
   `recent_trades` → `StoredTrade { id, fill, client_order_id }`. `OrderJournal` (`orders` table): row written
   `pending` *before* submit, `open` + venue id on ack, `closed` after last
   fill recorded → crash leaves row to check. `SqlStore::open` applies pending
   migrations; `bind_network` stamps db with network (testnet/demo/live),
   mismatch = `WrongNetwork` error: one db per network.
   `StrategyStateRepository` (`strategy_state`, migration 4): one row per
   strategy name = last `save_state` JSON + params hash + time. `cli::state`:
   params hash = SHA-256(name, params table); mismatch → fresh. Observer
   publishes changed state on `watch`, background task writes (engine never
   waits), flushed on exit; write failure = warning.
4. **Backtest** — `backtest` crate: load cached klines, replay as `MarketEvent`
   stream through engine, emit report (realized P&L, max drawdown, trade
   count, win rate, Sharpe). Fill model stated below, not hidden.
   `fetch-data` pulls closed klines + instrument filters from keyless
   `data-api.binance.vision` (`exchange::binance::PublicClient`, native-tls)
   into `data/klines/<venue>/<SYMBOL>-<interval>.jsonl` and
   `data/instruments/<venue>/<SYMBOL>.json`; only missing head/tail ranges
   downloaded, then offline. Reads re-validate (corrupt cache → error).
   Instrument file rewritten every fetch (incl. `price_band` from
   `PERCENT_PRICE_BY_SIDE`, else legacy `PERCENT_PRICE`; caps from
   `MAX_NUM_ORDERS` / `MAX_NUM_ALGO_ORDERS`; older files: none).
   Filters are today's, not historical — approximation (older, finer-tick
   prices: slipped fills rounded to today's coarser tick, e.g. SOL 2021 at
   ~1.84 with tick 0.01 ≈ 0.5% worse per taker fill). `KlineFeed` merges
   instruments ordered by (close_time, id). Warm-up like `run`: CLI backtest
   replays the last `--history-len` cached bars before `--from` first as
   `Backfill` (`BacktestProvider::warm_up_from_cache` / `with_warmup`;
   fewer cached → stderr warning); report starts at `--from`. Pinned by
   `tests/warmup.rs` (start at t3 + warm-up = fills of start at t0).
   Data check (`backtest::suspect_moves`, `BacktestProvider::all_candles`):
   before each run (CLI + console, `backtest_report`) bars where prev close →
   open/close or open → close moved > 30% by ratio (1.3×, either way) are
   warned per symbol (3 shown, rest counted), warm-up included; never
   corrected (splits unadjusted on eToro; real crashes move that much too).
   Delistings without lookahead (`backtest::listing`): announced ones from
   committed `config/delistings/<venue>.toml` (UTC `announced`, `at`,
   required `source`; replayed at announcement, candles from `at` dropped:
   reused symbol); unannounced stops: any gap > `--max-gap-hours` (48) or
   early end → cut, `Delisting { at: last close + 1 ms }` at that close (no
   notice, holding stuck at last price; LUNA classic/2.0 splice, FTT halt,
   SRM end). Earn: `--earn-ratio r` → `PaperBroker::with_earn_ratio` keeps
   r·balance unspendable (as live `[earn]` sweep holds it); `--earn-apr`
   (needs ratio) → `Report::compute_with_earn` / `EarnModel`: simple interest
   on r·ledger cash between close times, added to equity (not cash), line
   "earn interest … assumed APR"; never measured (rates unpublished).
   Never inferred early from where data ends. `--delist-exit-hours`
   = engine exit. PaperBroker on Delisting: from `at` cancels the coin's
   orders, refuses new (as venue). Pinned by `tests/delisting.rs`.
   `BacktestProvider` = cached feed + PaperBroker(NextOpenFillModel), through
   `engine::run`; `run_backtest` → `Report` (all `Decimal`). Report keeps own
   cash ledger (broker has none) replaying fills vs candles; equity = cash +
   holdings at latest close, once per close time. Max drawdown = fraction of
   running peak (starts at initial cash). `min_cash` = lowest cash after each
   instant's fills (same ts = simultaneous; replay order must not decide
   it); report ledger independent of broker → negative = broker let overspend through (bug).
   `BacktestProvider` takes `initial_cash` = PaperBroker balance (first
   instrument's quote; other-quoted orders refused) = report start. Closed trade = sell; win = realized
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
- PaperBroker refuses sells > holding − pending sells, and an order past
  the instrument's open-order caps (`exchange::check_open_count`, own
  working orders; BinanceBroker same, cancelling ones excluded), and what an
  optional `OrderGate` (`with_gate`: venue's live market status) says the
  venue would refuse (submit / cancel only; resting orders keep settling;
  backtest has no gate = always open). With balance
  (`with_cash`; backtest always) refuses unaffordable buy at submit like
  Binance `-2010` (→ `RunReport` rejection). Pending buy reserves max
  spend: quote budget = budget; limit/stop-limit = notional at limit +
  worse of maker/taker quote fee; market/stop by qty = last close (stop if
  higher) slipped + taker fee (no close yet → refused). Available =
  balance − reservations + pending market sells' expected proceeds (last
  close slipped, less fee): live market sell fills at once, buy right
  after spends it (rotations). Resting limit sells not counted. Sells
  never refused for quote. At fill buy checked vs ACTUAL cash (balance −
  other reservations, no unfilled sale counted); short but same-open
  market sells not yet settled (their coin's candle later in replay) would
  cover → buy waits (`deferred`, still open/cancellable), fills after them
  at its own candle, ts = that open; else / instant over → dropped
  unfilled (live `-2010`). Fills in live order, cash never negative.
- eToro (`TouchFillModel`, `BacktestProvider::with_touch_fills`, CLI
  `--venue etoro`): its resting limit is market-if-touched — first candle
  open at/through the limit → open; else a touch (`low <= limit` buy,
  `high >= limit` sell) → at the limit, taker, slippage NOT capped at the
  limit (`FillModel::caps_at_limit` false: it executes at market). IOC/FOK
  limits and stops refused at submit (`FillModel::accepts`; eToro's
  `limitIOC` ignored its limit on demo 2026-11-18).
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
baseline[A] = units sold. Sell A, then buy B with exact proceeds
(quote budget → never overspends) once sell done: venue may split order
into many fills (paper never does; one settle may carry all), so `on_fill`
only sums, leg ends when something filled + no order of ours working on
coin open or cancelling (checked in `on_candle`, after event's fills; a
fill can beat a cancel and be reported after it). Leg that started
filling never cancelled as stale / stopped (would split holding). Leg
placed by that check skips its close time's evaluation (not yet in
`open_orders`: timeout 1 would mark it stale, cancel nothing, never
re-price). `order_type` required: `market` (taker,
next open + slippage; leg unfilled 2 evals → stops) or `limit` (GTC at last
close, maker, estimate without slippage; needs `limit_timeout`: stale buy
cancelled by id via `ctx.open_orders`, once cancel final re-priced at close
for budget − spent; stale sell cancelled, once final → keep holding if
nothing sold). Sell leg idle with something sold but a sellable rest of
the coin left (fill beat the cancel, or restart reconcile) → sell the rest
at last close first, then buy with all proceeds (never two coins; dust
rest = sold). Limit may never fill: real cost.
State (phase, baselines, last eval) saved each event, restored on restart
checked vs books: Holding needs coin held; Selling + coin held → Holding
(reconcile cancelled sell, nothing sold); part sold → finish sell, then buy; Selling + coin gone → wait recovered fill → buy
with proceeds; Buying → re-place buy for budget − spent (recovered fills
counted) once nothing on coin can fill; Stopped →
Start. No/refused state: resume on most valuable sellable held coin
(baselines from that eval), else buy `quote_amount`. Flat start (every
backtest) unchanged.
Every built-in saves state (`builtin::STATE_VERSION` = 1): stateless ones
(sma_trend, sma_long_short, dual_ma_cross, donchian_breakout, atr_breakout, rsi_reversion, avwap,
volatility_breakout, rebalance, vol_target, bollinger_reversion, macd_cross,
ichimoku,
supertrend,
stochastic,
turn_of_month,
pair_reversion,
tsmom,
risk_parity) only
version;
buy_and_hold `decided`; dca `last_period` + `buys`; momentum_rotation
`last_eval`; top_k_momentum `rebalanced_at` (null = never: old state rebalances
at once); trailing_stop `started` + track (side, extreme, last close time); grid stage, gaps,
dust. Pinned by
`crates/backtest/tests/restart.rs`: every strategy, every idle cut point
(no working order): run₁ + save → fresh strategy + restore + Backfill
warm-up + same broker → fills identical to uninterrupted run. New strategy
→ add a case. Exception: grid always has orders working (no idle cut), so
its property lives in `tests/grid.rs`: cut, cancel all (as reconcile),
restore → rebuilds the identical book. volatility_breakout: no idle cut
either (stop or exit always working); stateless, restart misses ≤ 1 bar's
entry. Harness shared: `tests/common`.

Delisting in strategies (engine exit is the backstop): `rebalance::completes`
skips coins past their delisting (else multi-coin strategies halt for good);
`portfolio::rebalance` targets any delisting coin at 0 (sold at announcement);
risk_parity renormalizes over the rest; top_k_momentum / momentum_rotation
don't rank it; pair_reversion never switches into it; ratio_hop leaves a held
delisting coin for the best other one without beating its baseline, never
rotates into one; grid winds down via its stop path. Single-coin built-ins:
risk refuses their buys, engine exit sells.

## Other built-ins
`buy_and_hold` — the benchmark every strategy is measured against: can't beat
it on same coin after costs → no edge. Params `coin`, `quote_amount`, `fee`,
`slippage_bps` (costs required, range-checked, unused). Decides once on coin's
first candle: flat → quote-budget market buy; already held (restart) → hold.
Never sells; rejected buy not retried. Keep minimal.
`dca` — dollar-cost averaging, `buy_and_hold` spread over time: fixed
`quote_amount` quote-budget market buy on coin's first candle, then first
candle *closing* in each new UTC `every` period (`hour`/`day`/`week` from
Monday/`month`, `builtin::calendar`, shared with avwap). Optional
`max_buys`. Order on coin working → retry next candle same period; rejected
→ uncounted, period stays used (no `-2010` loop). Never sells. Removes
entry-timing risk, not direction risk; fee + slippage per buy.
`sma_long_short` — `sma_trend` both ways: long above SMA·(1+band), short
below SMA·(1−band) if `allows_short` (perp), else flat (spot = sma_trend
sized in base). `notional` (quote) → lot(notional/close) base at entry; flip
= one market order (held + new); dust = flat; idle only (open or
cancelling → nothing). Stateless. Leverage/margin = broker's, unseen.
Funding + whipsaw costs both sides; pinned on a perp through the engine
(`backtest` runner test: 2x flip back refused for margin, equity 484.8487).
`sma_trend` — one coin, long or flat (cash = defensive; v1 spot no short).
Long while close > SMA(`sma_period` closes, current bar included) ×
(1 + `band_pct`); cash below SMA × (1 − `band_pct`). Buy = quote-budget market
(`quote_amount`), sell = lot-rounded full holding. "Cash" = holding not
sellable under venue filters (`check_market`, every built-in) (dust from base fee counts as cash, else never
rebuys). No decision while an order on coin is open. `fee`/`slippage_bps`
required, range-checked, unused in decisions. `history_len` ≥ `sma_period`.

`momentum_rotation` — multi-coin, one bridge quote (else stops at `on_start`).
Once per close time when all coins' candles in (ratio_hop guard), none with
open order: return over `lookback` bars per coin (needs `lookback + 1`
closes each, else nothing); best (tie → first listed) is target if return >
`min_momentum` (default 0), else cash. Sell every held coin ≠ target (sells
first), buy target if not held. Entry = fixed `quote_amount`, not sale
proceeds: losing hop can't afford entry (backtest and live refuse,
`-2010`; stays cash). Bets winners persist; fails on sharp reversals, choppy
leadership. Dust = cash.

`macd_cross` — `dual_ma_cross` skeleton, level rule. MACD = EMA(fast) −
EMA(slow), signal = EMA(signal) of MACD; flat and MACD > signal → buy, long
and < → sell, equal → nothing. EMAs over fixed window of newest
3·slow + signal closes, mean-seeded (`indicator::ema_series`; whole-history
EMA would depend on where bounded history starts; seed weight < 1% at
12/26/9). `1 ≤ fast < slow`, signal ≥ 1. Whipsaws in ranges, lags turns.

`donchian_breakout` — `sma_trend` skeleton. Channel = highest high / lowest low
of `channel_period` candles *before* current (needs `channel_period + 1`).
Flat and close > high → buy; long and close < low → sell; edge → nothing.
Excluding current bar = what makes rule satisfiable (close ≤ own high), not
lookahead (bar closed). Bets breakouts continue; whipsaws in ranges.

`atr_breakout` — `sma_trend` skeleton, volatility-scaled. Window = last
`period` bars incl. current; ATR = `indicator::atr` (simple mean of true
ranges, needs `period + 1`). Flat and close > mean close + `entry_mult`·ATR
(Keltner-style) → buy; long and close < window's highest high −
`exit_mult`·ATR (chandelier, LeBeau) → sell; edge → nothing. Stop rolls
with the window (not a ratchet: old high leaves → stop drops). ATR because
noise is the constraint: fixed % too tight when volatile, too loose when
calm. False breakout ≈ (entry_mult + exit_mult)·ATR + round-trip costs.

`volatility_breakout` — Larry Williams, one coin, one-bar hold. Flat at a
close → buy `StopMarket` at close + `k`·(high − low) of that bar (24/7:
next open ≈ close; Williams used the open), quote → base at stop by risk
(gap fill spends a bit more). Long at a close → market sell all. Unfilled
stop cancelled + replaced same close (cancel routed first); live race can
fill both → next close sells all. Cancelling order on coin / own sell
working → nothing. Range 0 → no stop. Only built-in using stops (pins the
stop path, `tests/volatility_breakout.rs`). Cost: 2 taker fees + 2
slippages per trade; live stop at a fast breakout slips more than model.

`rebalance` — fixed `weights` (coin → fraction, sum ≤ 1, rest = cash, one
quote asset else stopped at `on_start`) of `capital`. Strategy can't see
balance (invariant 5) → own cash derived from books, stateless:
`capital − Σ(cost_basis − realized_pnl)` = net quote spent since books
empty (`builtin::portfolio`; BNB fee counted as quote → cash understated;
prior trades on same coins shift it). Once per close time all coins in,
nothing working/cancelling: any |weight − target| > `band` → all coins back
to target, market, sells first; buys ≤ cash + sells' est. proceeds
(close·(1 − fee − slip) − tick/unit), scaled together; filter-failing trades
dropped. Refusal (`-2010`: capital > account) → `portfolio::Backoff`: k
refused evals in a row skip next 2^k − 1 (≤ 63), fill resets, not saved
(restart retries). Volatility harvesting: sells rises, buys falls; trails a
one-way trend; costs per rebalance.

`vol_target` — one coin, `rebalance` machinery (`portfolio`, own cash from
books) with a moving weight: min(`max_weight` ≤ 1, `target_vol` /
(`indicator::return_sd` over `vol_period` returns × √(bars per 365-day
year of candle interval))); vol 0 → max. Each bar, nothing working: trade
to weight if > `band` off. Approx sqrt sets weight only; orders exact.
Bets on volatility clustering (cuts drawdown, not a return forecast);
late after crashes; trades as vol moves.

`ichimoku` — `long_flat` skeleton. `indicator::ichimoku`: tenkan /
kijun = high-low midpoints of last `tenkan` / `kijun` bars; cloud = span A
((tenkan + kijun)/2) and span B (midpoint of `senkou_b`) computed
`displacement` (default = kijun) bars back. Flat, close > cloud top and
tenkan > kijun → buy; long, close < cloud bottom → sell. Needs
displacement + max(periods) bars.

`supertrend` — `long_flat` skeleton. `indicator::supertrend`: hl2 ±
`mult`·ATR(`period`, simple), lower band only rises while close stays above
it, upper only falls; close < lower flips down, > upper flips up; recursion
over the newest `window` (default 100, ≥ period + 2) bars, seeded up
(fixed window: backtest/live/restart equal). Long while up.

`stochastic` — `long_flat` skeleton (shared single-coin helper: dust =
cash, no decision while order open). %K = (close − LL)/(HH − LL)·100 over
`k_period` (flat range → 50), %D = mean of last `d_period` %K
(`indicator::stochastic`, needs k + d bars: prev and current pair). Flat,
%K crosses above %D (prev ≤, now >) and %K < `oversold` → buy; long, cross
below and %K > `overbought` → sell. Mean reversion; knife risk.

`trailing_stop` — one coin, stateful. First candle flat → buy
`quote_amount` (already long → trail). Long (books: sellable, dust = out):
peak = highest close since long; close ≤ peak·(1 − `trail_pct`) → market
sell all. Out: trough = lowest close since out; close ≥ trough·(1 +
`reentry_pct`) → buy `quote_amount`. Side flip in books (or refusal) →
track restarts from current close (no `-2010` loop). Each decision folds
every history bar newer than last folded close time → Backfill (restart
warm-up, reconnect gap) still moves the peak. Restore takes state as is
(side flip handled by next decision). Drawdown cap; chop = round trips.

`risk_parity` — `rebalance` machinery, ≥ 2 coins (one quote), weights
moving: `exposure`·(1/sd_i)/Σ(1/sd_j), sd = `indicator::return_sd` over
`vol_period` returns (per-bar: ratios need no annualizing); any sd 0 → no
decision. Once per close time all coins in, idle (`portfolio::idle`).
Naive (no correlation). Lowers concentration, predicts nothing; trails the
winner; trades as relative vol moves.

`pair_reversion` — exactly 2 coins A, B (one quote), `portfolio`
machinery. Ratio r = close A / close B over last `period` aligned close
times (misaligned → no decision); population var v, d = r_now − mean:
d < 0 and d² > `entry_z`²·v → hold A; d > 0 and same → hold B (exact, no
sqrt); else keep held (books: more valuable sellable coin; none → cash).
Held = weight 1 of `capital`, other 0 → switch = sell + buy with est.
proceeds, one eval. Stateless. Plain ratio, not log. Always long the
pair's common move (no short); ratio trends → keeps buying the loser.

`top_k_momentum` — `momentum_rotation` with k slots on `portfolio`
machinery. ≥ 2 coins (one quote), 1 ≤ `k` < coins. Rank by close /
close[`lookback` ago] − 1 (stable: tie → listed first); best k with return
> `min_momentum` (default 0, > −1) get 1/k of `capital`, empty slot = cash.
Once per close time all in, idle; any weight > `band` off → all back to
target (rotation = sell + buy from est. proceeds, one eval). Overlays, all
optional (defaults = the above): `lookbacks` (instead of `lookback`,
exactly one) → momentum = mean of the windows' returns; `trend_period`
(≥ 2) → chosen only if close > SMA(n incl. current), a failing top-k
coin's slot = cash (not passed down); `weighting = "inverse_vol"` +
`vol_period` (≥ 2) → chosen share (count/k) split by 1/`return_sd` (sd 0 →
no decision; relative, not an exposure cut); `rebalance_every` (calendar
`Period`) → rank + rebalance only at first eval of a new period
(`rebalanced_at` = that eval's close, set when `portfolio::rebalance`
answers), between them `trail_pct` (needs schedule) sells the whole
sellable holding when close ≤ max close since `rebalanced_at` ·
(1 − trail), slot cash until next period (window from history:
`history_len` must span a period). Restart case with every overlay.

`turn_of_month` — `long_flat` skeleton, calendar rule. Window (UTC, a bar's
date = its close time's): bar day > days in month − `enter_days` (1–10) →
long; else long while fewer than `hold_bars` (1–15) bars of the new month
have closed, counted only if history reaches the previous month (a
history starting mid-month reads as outside: no buy on a short warm-up);
else cash. `calendar::day_of_month`. Stateless. Fixture
`turn_of_month` (760 1h bars over Jan 31 → Feb 1 1970) for restart.
Measured: eToro SPY 2020–2026 +28% vs hold +133%, dd 9.7% vs 33% (in
market ~1/4 of the time; no excess return).

`tsmom` — Moskowitz/Ooi/Pedersen time-series momentum, long-only,
`portfolio` machinery, ≥ 1 coin (one quote). Per coin: close /
close[`lookback` ago] − 1 > 0 (= 0 → out) → min(`max_weight`, `target_vol`
/ annualized `return_sd` over `vol_period`) / N, else 0; needs
max(lookback, vol_period) + 1 closes for every coin. Once per close time
all in, idle. Trend + vol targeting = the two drawdown cutters our
backtests found. Chop, lag, trades as vol moves.

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
to the bottom (no stop, by design). Optional `trend_period` (Connors RSI-2):
buy only if close > SMA(trend_period closes incl. current; too few → no buy);
gates entries only, never exits. Dust = cash; no decision while order open.

`bollinger_reversion` — `rsi_reversion` skeleton. Window `period` closes incl.
current: mid = mean, var = population variance. Flat and close < mid −
`k`·sd → buy; long and close > mid → sell. Band test without sqrt: close <
mid and (mid − close)² > k²·var (exact; on the band ≠ below). Same knife
risk as rsi_reversion (no stop).

`avwap` — `sma_trend` skeleton, anchored VWAP instead of SMA. AVWAP =
Σ quote_volume / Σ volume from anchor bar through current (exact via
`Instrument::price_for`, not typical-price approx). `anchor` required:
`day` / `week` (Monday) / `month` = first bar of current UTC period (must
be in history, else no decision; month via Hinnant civil-date math);
`swing_low` / `swing_high` + `swing_lookback` = lowest low / highest high
of last N bars incl. current (tie → most recent). Long above
avwap·(1+`band_pct`), cash below avwap·(1−`band_pct`). Zero volume / no
anchor / order open → no decision. Dust = cash.

`grid` — one coin, spot grid. `grids + 1` levels split [`lower`, `upper`]
(`spacing` `arithmetic` | `geometric`, required); gap i buys
`quote_per_grid` at Lᵢ (GTC limit), its lot (net of base fee) sells at
Lᵢ₊₁. Build refuses a grid whose narrowest step doesn't clear two `fee`s
(Lᵢ₊₁·(1−fee)² > Lᵢ); `on_start` re-checks on the tick (levels rounded at
12 dp then floored, `powd` approximate), that each buy passes filters and
`grids` ≤ `max_open_orders` (one order per gap), else Stopped
(every stop logged `warn` with reason: stopped on candle 1 ≠ silent run). First eval: every gap buys, those above price marketable
(neutral opening). Order ↔ gap via `on_order` ids (paper order can fill
before visible); fills sum, gap settles when its order neither open nor
cancelling (split fills); refusal (`on_rejected`) → gap back, no new orders
until next fill (no `-2010` loop). Lot remainders → `dust`. Restart /
 fresh-start credit capped at the GROSS lot (not net of fee: on a quote-fee
 venue like eToro, a net cap left an unsellable crumb in dust). Venue price band (`price_band`, avg from
history): buy above top → placed at top, sell below bottom → at bottom
(marketable either way); buy below bottom / sell above top → gap waits
unplaced. Optional
`stop_price` < `lower`: close below → cancel all, once idle market-sell
all, Stopped. Restart (reconcile cancelled all; position already has
downtime fills): working gaps settled, squared with position (extra coin →
buys that were working, nearest price first; missing → sells that were
working, lowest first); unknown-id fills ignored; unsquarable → refused →
fresh start gives held coin to gaps above price. Bets on oscillation in
range; loses to trends both ways (bags all levels falling, sells out
rising). Edge unverified.

## Layout
```
Cargo.toml              workspace
crates/
  core/        (package `bot-core`: `core` clashes with Rust built-in) Price/Qty/Notional, ids, Instrument, Candle, MarketEvent, Signal, Order, Fill, Position
  strategy/    Strategy trait, Context, inventory registry, `script` (feature `rhai`:
               script strategies), built-ins (atr_breakout, avwap, bollinger_reversion,
               buy_and_hold, dca, donchian_breakout, dual_ma_cross, grid, ichimoku, macd_cross,
               momentum_rotation, pair_reversion, ratio_hop, rebalance, risk_parity, rsi_reversion,
               sma_long_short, sma_trend, stochastic, supertrend, top_k_momentum, trailing_stop, tsmom, turn_of_month, vol_target,
               volatility_breakout)
               `indicator` (shared, window of newest bars only: ATR = simple mean of
               true ranges, not Wilder: Wilder depends on where bounded history starts;
               return_sd = sample sd of simple returns, approx. sqrt: sizing only;
               stochastic / stochastic_k, supertrend (fixed-window recursion),
               ichimoku (displaced spans: past data only) over `&[&Candle]`;
               ema_series = mean-seeded EMA, callers pass a fixed newest window)
  exchange/    Provider + MarketData + Broker traits; binance{testnet,demo,live}; paper broker; binance ws;
               max (MaiCoin: public data + kline ws so far)
  engine/      shared event loop (backtest + live)
  backtest/    kline loader, BacktestProvider, runner, report
  storage/     SeaORM entities + repositories
  api/         read-only HTTP API (axum): /status /positions /open-orders /orders /trades;
               no write route ever (orders only from strategy via risk)
  cli/         binary: run | backtest | fetch-data | migrate | console
               (`console`: axum on loopback only, page `console.html` embedded;
               catalog / scripts get+put / example params / check / backtest via
               `backtest_report` (= `backtest` command) + buy_and_hold benchmark;
               guard: Host = own loopback addr, Origin same, writes JSON only;
               writes only `<scripts_dir>/<[a-z0-9_]{1,64}>.rhai`; one backtest at
               a time; no keys, no orders; `Report.equity_curve` for the chart)
migration/     SeaORM migrations
strategies/    script strategies (`sma_trend.rhai`: reference port, tested vs built-in)
config/        config.example.toml (committed); *.local.toml (gitignored). Env (BOT_*) overrides file.
  examples/    one runnable config per built-in (`<strategy>.toml`, paper/testnet), README
               `etoro/`: venue examples (demo configs + `params/<strategy>-<what>.toml`),
               tested by `venue_examples_load` (not part of one-per-strategy)
data/          downloaded klines, per-network run dbs (gitignored)
.github/       ci.yml (`make check` + deploy configs via each tool's image), release.yml
               (tag `v<workspace version>` → `trade-bot` for linux x86_64/arm64 gnu,
               macOS arm64/x86_64, windows x86_64; `cli --features vendored-tls,rhai`
               (static OpenSSL + script strategies; archive ships `strategies/`);
               archives `trade_bot_<os>_<arch>.<version>.tar.gz` (windows `.zip`;
               os linux|darwin|windows, arch amd64|arm64) + SHA256SUMS on a GitHub
               release), pages.yml
               (site/ → GitHub Pages sc0vu.github.io/trade-bot on push to main)
site/          static project page (one index.html, no build); strategy table and
               safety claims mirror README: keep in sync
Dockerfile     debian + rustup (toolchain pin), system libssl, `--features rhai`
               (+ `strategies/` at /app/strategies); `run --paper` default,
               books in volume /app/data; live only via typed command (invariant 7)
render.yaml    worker, frankfurt (Binance 451s US IPs), disk /app/data (books survive;
               no overlapping instances), maxShutdownDelaySeconds 120 (close-out)
deploy/        Prometheus + Alertmanager + Loki/Alloy (tails `[log] file`) + Grafana
               compose, alerts.yml (+ promtool unit tests alerts_test.yml), dashboard;
               keep in sync with metric names and log keys (cli test pins alloy keys)
doc/           architecture.md (human-facing; update with design changes)
README.md      user-facing overview, quick start, config, limitations
```

## Commands
`make` lists targets (`etoro-stocks-data` / `etoro-stocks-backtest`: eToro stock examples
vs buy-and-hold SPY; `HISTORY_LEN` optional; `check` = fmt-check + lint + doc + test, run before commit;
`migrate-*`, `fetch-data`, `backtest`, `run-paper`; vars `DATABASE_URL`,
`VENUE` (binance|max), `SYMBOL`, `FROM`, `FEE`, `INITIAL_CASH`, …). No live target on purpose: `--i-understand` must be
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
  gitignored) or env; only `config.example.toml` and `config/examples/`
  committed (both loaded by tests, so they can't rot: every example loads,
  its strategy builds, file named after it, one per registered strategy →
  new strategy = add `config/examples/<name>.toml`). `exchange.network` defaults `testnet`. No field exists
  for a secret: `api_key` in a file = unknown-field error.
- `[log] level` (`run` only): `RUST_LOG`-syntax filter, validated on load;
  precedence `RUST_LOG` > config (`BOT_LOG__LEVEL` > file) > default
  `info,sqlx=warn`. `run` loads config before logging starts (loading
  logs nothing). backtest / fetch-data: `RUST_LOG` else `warn`.
  `[log] format` `text` (default) | `json` (console); `[log] file` = also
  append JSON lines (`cli::logging`: flat `timestamp`, `level`, `target`,
  `message`, fields — keys `deploy/alloy` parses) for a shipper; open
  failure = warning. Fatal run error also logged (`run stopped`). Engine
  order/cancel/fill lines log `Broker::client_order_id` (log only, never
  a decision): Binance `<run tag>-o-N`, paper the engine id.

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
`tokio, async-trait, inventory, rhai (optional), reqwest, tokio-tungstenite, serde/serde_json,
rust_decimal, sea-orm, sea-orm-migration, figment, thiserror, anyhow, tracing, clap, toml,
metrics, metrics-exporter-prometheus, metrics-process, axum`.
(`barter-rs` option if later prefer ready-made framework over
hand-rolled traits; `rhai`/`wasmtime` when strategy scripting needed.)