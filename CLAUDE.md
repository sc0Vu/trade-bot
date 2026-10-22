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
   requires `mode = "live"`, non-testnet provider, AND explicit
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
    async fn market_data(&self) -> Box<dyn MarketData>;
    async fn broker(&self) -> Box<dyn Broker>;
    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>>; // reference data
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
accounting rejects `Other`. Positions average cost, fees included;
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
    fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal>;
    // convenience default for candle-only strategies:
    fn on_candle(&mut self, _ctx: &mut Context, _c: &Candle) -> Vec<Signal> { vec![] }
    fn on_fill(&mut self, _ctx: &mut Context, _f: &Fill) {}
}

#[async_trait] trait MarketData { async fn next(&mut self) -> Option<MarketEvent>; }
#[async_trait] trait Broker {
    async fn submit(&mut self, o: Order) -> Result<OrderId>;
    async fn cancel(&mut self, id: OrderId) -> Result<()>;
    fn position(&self, i: &InstrumentId) -> Position;
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
1. **Binance testnet + live** — `BinanceSpotProvider` in `exchange::binance`.
   One client; base URL, keys from config (`testnet: bool`). Testnet host
   `testnet.binance.vision`. Same signing, same code path. More venues =
   new `Provider` impls, nothing else changes.
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
4. **Backtest** — `backtest` crate: load cached klines, replay as `MarketEvent`
   stream through engine, emit report (realized P&L, max drawdown, trade
   count, win rate, Sharpe). Fill model stated below, not hidden.
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
- market order → fills at **next** candle's open.
- limit buy → fills if `candle.low <= limit`, at limit (or open if gapped
  through); sell symmetric.
- stop → triggers if `candle.high >= stop` (buy) / `candle.low <= stop` (sell).
- fee (taker/maker) + configurable slippage every fill.
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

## Layout
```
Cargo.toml              workspace
crates/
  core/        (package `bot-core`: `core` clashes with Rust built-in) Price/Qty/Notional, ids, Instrument, Candle, MarketEvent, Signal, Order, Fill, Position
  strategy/    Strategy trait, Context, inventory registry, built-ins (ratio_hop)
  exchange/    Provider + MarketData + Broker traits; binance{testnet,live}; paper broker; binance ws
  engine/      shared event loop (backtest + live)
  backtest/    kline loader, BacktestProvider, runner, report
  storage/     SeaORM entities + repositories
  cli/         binary: run | backtest | fetch-data | migrate
migration/     SeaORM migrations
config/        config.example.toml (committed); *.local.toml (gitignored). Env (BOT_*) overrides file.
data/          downloaded klines (gitignored)
```

## Commands
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
   (`BOT_EXCHANGE__TESTNET=true`, `BOT_STRATEGY__NAME=ratio_hop`,
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
  requires non-testnet provider + `--i-understand` gate. Bad config
  never reaches trading loop.
- Real config = `config/*.local.toml` (gitignored) or env; only
  `config.example.toml` committed.

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
- Every strategy and fill model get unit tests. Backtester gets
  known-input regression test so refactor can't silently change P&L.
- Honest claims: number not measured (backtest return, latency) →
  label unverified. Mirrors owner's first-principles preference.

## Crates
`tokio, async-trait, inventory, reqwest, tokio-tungstenite, serde/serde_json,
rust_decimal, sea-orm, sea-orm-migration, figment, thiserror, anyhow, tracing, clap, toml`.
(`barter-rs` option if later prefer ready-made framework over
hand-rolled traits; `rhai`/`wasmtime` when strategy scripting needed.)