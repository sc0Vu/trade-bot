# CLAUDE.md

## Project
Rust trading bot. First venue Binance (crypto spot); architecture multi-venue, multi-asset by design so equities follow without rewrite.
Event-driven, strategy-pluggable, backtester runs **same** strategy code as live. v1 scope: paper/backtest-correct on crypto spot; real
orders gated behind explicit config.

Scope ladder (build in order, don't foreclose later rungs):
**crypto spot now → US equities next → options later (deferred, not in v1).**
Domain split so equities/options *can* slot in; only implement crypto spot now.

Built with Claude Code. Owner reasons from first principles: on design choice, name governing constraint (fees, latency, lookahead,
precision, settlement), don't copy another bot. Don't accept strategy's stated edge at face value — derive why it holds or not after costs.

## Non-negotiable invariants
Correctness, not style. If task violates one, stop and flag.
1. **Money is `rust_decimal::Decimal`.** Never `f64`/`f32` for price, quantity,
   balance, fees. Binary float silently loses cent fractions, compounded
   over thousands of trades.
2. **No lookahead.** Strategy reads market data only up to and including
   current event. Never index future bar. This is *why* engine is
   event-driven, not vectorized over full DataFrame.
3. **Backtest == live.** `Strategy` trait and engine loop shared.
   Backtest swaps only provider (data feed + broker). If change makes
   backtest and live diverge in strategy-visible behavior, change is wrong.
4. **Asset-class math lives in `Instrument`, not in `Broker` or `Strategy`.**
   Notional, settlement, multiplier, expiry belong to instrument. Broker
   submits orders, reports fills; never needs to know how to value one.
5. **A strategy's only window to the world is `Context`.** Position, rolling
   history, params, submit-order — never direct handle to broker, provider,
   or network. Lets strategy-import mechanism evolve (registry →
   script → wasm) without touching strategy code.
6. **Secrets via env only.** API keys from env vars named in config; never
   hard-code or commit. `.env` and `config/*.local.toml` gitignored.
7. **Safe by default.** Default mode paper against testnet. Live trading
   requires `mode = "live"`, non-testnet provider, AND explicit
   `--i-understand` run flag. Three locks — mistake here spends real money.

## Architecture
One-way data flow. Strategy is pure decision; provider is only thing
differing between backtest and live.

```
Provider (BinanceSpot | Alpaca | Backtest) ── supplies ──> MarketData + Broker
         │
MarketData ──MarketEvent──> Engine loop ──> Strategy.on_event() ──Signal──> Risk ──Order──> Broker
                                   ▲                                                           │
                              Context (position, rolling history, params) <────────Fill───────┘
                                                                                         └─> Storage (SeaORM)
```

**Provider** = venue + capabilities. Mints `MarketData` feed and
`Broker`, declares served asset classes, so engine can
reject equity strategy pointed at crypto-only venue before trading:

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

struct Instrument { id: InstrumentId, class: AssetClass, /* base/quote, tick, lot, ... */ }
impl Instrument {
    fn notional(&self, qty: Decimal, price: Decimal) -> Decimal;  // qty*price now; *multiplier for options later
    fn settlement(&self) -> Settlement;                           // Instant for spot; T+2 for equity
}
```

Strategy reacts to **MarketEvent**, not only candle, so loop already
survives non-price events equities/options bring (session boundaries,
corporate actions, expiry). v1 emits `Candle` only; other variants exist so
adding later is additive, not rewrite:

```rust
enum MarketEvent { Candle(Candle), Quote(Quote), SessionOpen, SessionClose,
                   CorporateAction(CorpAction) /*, Expiry(InstrumentId) deferred */ }

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

Engine generic over `Provider`. Backtest = `BacktestProvider`
(kline replay + PaperBroker). Live = `BinanceSpotProvider` (ws + BinanceBroker).
One loop, one strategy, provider swapped — that identity is whole correctness
story. Backtest must replay same `MarketEvent` stream live produces, so parity
holds for every event type, not just candles.

Loop order matters: each event, broker settles pending orders against
NEW event *before* strategy runs, so order decided on event t only fills
at t+1. Prevents deciding on close price and also trading at it.

## Strategy import — the Rust-specific problem
Python imports `.py` at runtime; Rust compiled, so "drop in a strategy" has
four real answers. v1 uses registry; rest stay open because of invariant 5.

| Approach | Recompile host? | User needs | Perf | Sandboxed | Use when |
|---|---|---|---|---|---|
| **Registry** (`inventory`) | yes | Rust + source tree | native | n/a | v1; strategies by you/contributors |
| **cdylib** (`libloading`) | no | Rust, exact compiler ver. | native | no (ABI/UB) | rarely |
| **Rhai script** | no | just write a script | interpreted | yes | external users, candle freq. |
| **WASM** (`wasmtime`) | no | compile to wasm | near-native | yes | external users, sandbox + speed |

v1: `inventory` so strategy file self-registers (`inventory::submit!`); adding
one = adding file — no central match to edit — runs in backtest free.
`strategy.params` is generic table each strategy deserializes into own typed
struct via serde (same for compiled and, later, scripted). Rhai is
later "drop a script" path; WASM serious sandboxed/polyglot upgrade. cdylib
only if native speed AND binary distribution both required (Rust has no stable
ABI — compiler mismatch is UB).

## Requirements → where they live
1. **Binance testnet + live** — `BinanceSpotProvider` in `exchange::binance`.
   One client; base URL and keys from config (`testnet: bool`). Testnet host
   `testnet.binance.vision`. Same signing, same code path. More venues =
   new `Provider` impls, nothing else changes.
2. **Coin whitelist** — `config.whitelist: Vec<InstrumentId>` is only universe
   bot may trade. Plus `filters.min_24h_quote_volume` drops illiquid
   instruments: thin alts slip badly, may not fill, so filter is safety
   feature. freqtrade's pairlist idea — static list + volume filter for v1.
3. **SQLite now, MySQL later** — `storage` crate on **SeaORM** (backend-agnostic
   entities; switch `db.url` from `sqlite://` to `mysql://`, no model changes).
   Repository traits wrap it so call sites never know backend. (sqlx lighter
   but needs per-dialect SQL; SeaORM chosen for MySQL path + migrations below.)
4. **Backtest** — `backtest` crate: load cached klines, replay as `MarketEvent`
   stream through engine, emit report (realized P&L, max drawdown, trade
   count, win rate, Sharpe). Fill model stated below, not hidden.
5. **User strategies** — implement `Strategy`, self-register via `inventory`.
   Config selects `strategy.name` + `strategy.params`. Trait
   provider-agnostic, so every user strategy runs in backtest free. See
   import ladder above for growth.
6. **Migrations** — SeaORM migration crate under `migration/`, run via
   `sea-orm-cli migrate up|down|fresh`. Every schema change is migration; never
   hand-edit live DB.
7. **freqtrade lessons applied** — config-driven; strategy fully decoupled from
   venue (via `Provider`); backtest/live parity as core correctness property;
   event-driven design structurally prevents lookahead bias freqtrade only
   detects after the fact with `lookahead-analysis`.

## Backtest fill model (assumptions are explicit)
Candle = four prices, can't say whether high or low came first. So:
- market order → fills at **next** candle's open.
- limit buy → fills if `candle.low <= limit`, at limit (or open if gapped
  through); symmetric for sell.
- stop → triggers if `candle.high >= stop` (buy) / `candle.low <= stop` (sell).
- fee (taker/maker) + configurable slippage on every fill.
- **intra-bar ambiguity**: stop and target both inside one candle →
  model can't know which hit first, assumes worse case. Finer-timeframe
  replay = future improvement. Backtest P&L is model output, never promise.

## Reference strategy
`ratio_hop` — coin-accumulation strategy from `ccxt/binance-trade-bot`: hold
coin strong vs peers, rotate `A → bridge → B` only when post-fee
ratio beats last-held amount of B, so unit count only ratchets up. Ships with
**freefall guard the original lacks**: refuse rotating into coin down more
than `risk.freefall_pct` over last N candles, so reverse-greedy loop can't
accumulate coin collapsing to zero. Worked example of trait, and
reminder: maximizing units ≠ maximizing value.

## Layout
```
Cargo.toml              workspace
crates/
  core/        (package `bot-core`: `core` clashes with Rust built-in) Instrument, AssetClass, Candle, MarketEvent, Order, Signal, Position, Money(Decimal)
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
sea-orm-cli migrate up                            # apply migrations
cargo test && cargo clippy -- -D warnings && cargo fmt --check
```

## Configuration
Layered; each source overrides one above:
1. built-in defaults (in code)
2. config file — `config/config.toml` (path overridable via `--config` or
   `BOT_CONFIG`); `config/config.example.toml` committed, documents every field
3. env vars, prefixed `BOT_`, nested with `__`
   (`BOT_EXCHANGE__TESTNET=true`, `BOT_STRATEGY__NAME=ratio_hop`,
   `BOT_DB__URL=sqlite://bot.db`) — Docker/Heroku/CI path
4. CLI flags (`--paper`, `--live`, `--strategy`, …)

- Loader: `figment` — TOML provider + `Env::prefixed("BOT_").split("__")` —
  deserialized into one typed `Config` struct via serde with
  `#[serde(deny_unknown_fields)]` so typo errors, not silently defaults.
  (`config` crate is alternative.)
- **Secrets are the exception to the file layer (invariant 6).** `api_key` /
  `api_secret` read ONLY from fixed env vars (`BINANCE_API_KEY`,
  `BINANCE_API_SECRET`), never in any TOML. File may name which env var
  to read, never hold value. Missing in live mode → hard error at startup.
- Validate on load, fail fast: whitelist non-empty, fee/slippage ≥ 0, live mode
  requires non-testnet provider + `--i-understand` gate. Bad config
  never reaches trading loop.
- Real config is `config/*.local.toml` (gitignored) or env; only
  `config.example.toml` committed.

## Definition of done (every change)
Change not finished until all hold — apply per task, not at end:
- Compiles; `cargo clippy -- -D warnings` and `cargo fmt --check` pass.
- **Tests ship in the same change.** Unit tests for new logic; regression test
  if touching backtester's P&L path or fill model. No test = not done.
  Assert against *expected* values derived independently —
  never assertion mirroring whatever code currently returns (passes
  vacuously, locks in bugs). For fill model and `ratio_hop`,
  owner supplies or confirms expected numbers.
- **Docs in the same change.** Public items (traits, structs, `pub fn`) carry `///`
  comments stating intent and invariants, not restating signature. Stale docs
  are bug: update with code.
- If change alters architecture, invariant, or config field, update
  CLAUDE.md and `config.example.toml` in same change.

## Commits
- One commit = one logical change. Feature, fix, refactor, rename are
  separate commits even in same session — never bundled.
- Commit as you go, not once at end. Unit complete (code + test
  + docs, per Definition of done) → commit before starting next. This,
  not size rule, keeps diffs small.
- Keep mechanical apart from behavioral: formatting-only or rename
  commit never also changes logic, so review stays on logic.
- Each commit builds and passes tests alone (don't break `git bisect`).
- Conventional Commit subject, imperative, ≤72 chars: `feat(storage): add trade_stats`.
  Body says why, not what diff shows.
- Never commit secrets, `*.local.toml`, or `data/`. Never `--force` shared branch.
- Don't commit unless asked or task clearly calls for it. If change spans
  several logical units, propose split first, not one big commit.

## Conventions
- Errors: `thiserror` in libraries, `anyhow` in binary. No `unwrap()` outside
  tests.
- Async `tokio`; JSON `serde`; money `rust_decimal`; logs `tracing` (structured,
  no money values at info level).
- Every strategy and fill model get unit tests. Backtester gets
  known-input regression test so refactor can't silently change P&L.
- Keep claims honest: number not measured (backtest return, latency) →
  label unverified. Mirrors owner's first-principles preference.

## Crates
`tokio, async-trait, inventory, reqwest, tokio-tungstenite, serde/serde_json,
rust_decimal, sea-orm, sea-orm-migration, figment, thiserror, anyhow, tracing, clap, toml`.
(`barter-rs` option if later prefer ready-made framework over
hand-rolled traits; `rhai`/`wasmtime` when strategy scripting needed.)