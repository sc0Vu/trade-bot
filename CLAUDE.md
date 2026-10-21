# CLAUDE.md

## Project
Rust trading bot. First venue is Binance (crypto spot); the architecture is
multi-venue and multi-asset by design so equities can follow without a rewrite.
Event-driven, strategy-pluggable, with a backtester that runs the **same**
strategy code as live. v1 scope is paper/backtest-correct on crypto spot; real
orders are gated behind explicit config.

Scope ladder (build in this order, don't foreclose later rungs):
**crypto spot now → US equities next → options later (deferred, not in v1).**
We split the domain so equities and options *can* slot in, but we only implement
crypto spot now.

Built with Claude Code. The owner reasons from first principles: when a design
choice is made, name the governing constraint (fees, latency, lookahead,
precision, settlement) rather than copying another bot. Don't accept a strategy's
stated edge at face value — derive why it would or wouldn't hold after costs.

## Non-negotiable invariants
Correctness, not style. If a task would violate one, stop and flag it.
1. **Money is `rust_decimal::Decimal`.** Never `f64`/`f32` for price, quantity,
   balance, or fees. Binary float silently loses fractions of a cent, compounded
   over thousands of trades.
2. **No lookahead.** A strategy reads market data only up to and including the
   current event. Never index a future bar. This is *why* the engine is
   event-driven rather than vectorized over a full DataFrame.
3. **Backtest == live.** The `Strategy` trait and the engine loop are shared.
   Backtest swaps only the provider (data feed + broker). If a change makes
   backtest and live diverge in strategy-visible behavior, the change is wrong.
4. **Asset-class math lives in `Instrument`, not in `Broker` or `Strategy`.**
   Notional, settlement, multiplier, expiry belong to the instrument. A broker
   submits orders and reports fills; it must never need to know how to value one.
5. **A strategy's only window to the world is `Context`.** Position, rolling
   history, params, submit-order — never a direct handle to the broker, provider,
   or network. This is what lets the strategy-import mechanism evolve (registry →
   script → wasm) without touching strategy code.
6. **Secrets via env only.** API keys come from env vars named in config; never
   hard-code or commit them. `.env` and `config/*.local.toml` are gitignored.
7. **Safe by default.** Default mode is paper against testnet. Live trading
   requires `mode = "live"`, a non-testnet provider, AND an explicit
   `--i-understand` run flag. Three locks, because a mistake here spends real money.

## Architecture
One-way data flow. The strategy is pure decision; the provider is the only thing
that differs between backtest and live.

```
Provider (BinanceSpot | Alpaca | Backtest) ── supplies ──> MarketData + Broker
         │
MarketData ──MarketEvent──> Engine loop ──> Strategy.on_event() ──Signal──> Risk ──Order──> Broker
                                   ▲                                                           │
                              Context (position, rolling history, params) <────────Fill───────┘
                                                                                         └─> Storage (SeaORM)
```

A **Provider** is a venue + its capabilities. It mints the `MarketData` feed and
the `Broker`, and declares which asset classes it serves, so the engine can
reject an equity strategy pointed at a crypto-only venue before trading:

```rust
#[async_trait] trait Provider {
    fn id(&self) -> &str;
    fn asset_classes(&self) -> &[AssetClass];           // Spot now; UsEquity later
    async fn market_data(&self) -> Box<dyn MarketData>;
    async fn broker(&self) -> Box<dyn Broker>;
    async fn resolve(&self, q: &InstrumentQuery) -> Result<Vec<Instrument>>; // reference data
}
```

An **Instrument** carries the asset-class math (invariant 4). v1 implements
`Spot` only; the enum leaves room for `UsEquity` (and, later, `UsOption`) without
reshaping callers:

```rust
enum AssetClass { Spot, UsEquity /*, UsOption (deferred) */ }

struct Instrument { id: InstrumentId, class: AssetClass, /* base/quote, tick, lot, ... */ }
impl Instrument {
    fn notional(&self, qty: Decimal, price: Decimal) -> Decimal;  // qty*price now; *multiplier for options later
    fn settlement(&self) -> Settlement;                           // Instant for spot; T+2 for equity
}
```

The strategy reacts to a **MarketEvent**, not only a candle, so the loop already
survives the non-price events equities/options bring (session boundaries,
corporate actions, expiry). v1 emits `Candle` only; the other variants exist so
adding them later is additive, not a rewrite:

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

The engine is generic over a `Provider`. Backtest = a `BacktestProvider`
(kline replay + PaperBroker). Live = `BinanceSpotProvider` (ws + BinanceBroker).
One loop, one strategy, provider swapped — that identity is the whole correctness
story. Backtest must replay the same `MarketEvent` stream live produces, so parity
holds for every event type, not just candles.

Loop order matters: on each event the broker settles pending orders against the
NEW event *before* the strategy runs, so an order decided on event t can only fill
at t+1. This prevents deciding on a close price and also trading at it.

## Strategy import — the Rust-specific problem
Python imports a `.py` at runtime; Rust is compiled, so "drop in a strategy" has
four real answers. v1 uses the registry; the rest stay open because of invariant 5.

| Approach | Recompile host? | User needs | Perf | Sandboxed | Use when |
|---|---|---|---|---|---|
| **Registry** (`inventory`) | yes | Rust + source tree | native | n/a | v1; strategies by you/contributors |
| **cdylib** (`libloading`) | no | Rust, exact compiler ver. | native | no (ABI/UB) | rarely |
| **Rhai script** | no | just write a script | interpreted | yes | external users, candle freq. |
| **WASM** (`wasmtime`) | no | compile to wasm | near-native | yes | external users, sandbox + speed |

v1: `inventory` so a strategy file self-registers (`inventory::submit!`) and adding
one is adding a file — no central match to edit — and it runs in backtest for free.
`strategy.params` is a generic table each strategy deserializes into its own typed
struct via serde (identical for compiled and, later, scripted strategies). Rhai is
the later "drop a script" path; WASM the serious sandboxed/polyglot upgrade. cdylib
only if native speed AND binary distribution are both required (Rust has no stable
ABI — a compiler mismatch is UB).

## Requirements → where they live
1. **Binance testnet + live** — a `BinanceSpotProvider` in `exchange::binance`.
   One client; base URL and keys from config (`testnet: bool`). Testnet host is
   `testnet.binance.vision`. Same signing, same code path. Additional venues are
   new `Provider` impls, nothing else changes.
2. **Coin whitelist** — `config.whitelist: Vec<InstrumentId>` is the only universe
   the bot may trade. Plus `filters.min_24h_quote_volume` to drop illiquid
   instruments: thin alts slip badly and may not fill, so the filter is a safety
   feature. This is freqtrade's pairlist idea — static list + volume filter for v1.
3. **SQLite now, MySQL later** — `storage` crate on **SeaORM** (backend-agnostic
   entities; switch `db.url` from `sqlite://` to `mysql://` with no model changes).
   Repository traits wrap it so call sites never know the backend. (sqlx is lighter
   but needs per-dialect SQL; SeaORM chosen for the MySQL path + migrations below.)
4. **Backtest** — `backtest` crate: load cached klines, replay as a `MarketEvent`
   stream through the engine, emit a report (realized P&L, max drawdown, trade
   count, win rate, Sharpe). Fill model stated below, not hidden.
5. **User strategies** — implement `Strategy`, self-register via `inventory`.
   Config selects `strategy.name` + `strategy.params`. Because the trait is
   provider-agnostic, every user strategy runs in backtest for free. See the
   import ladder above for how this grows.
6. **Migrations** — SeaORM migration crate under `migration/`, run via
   `sea-orm-cli migrate up|down|fresh`. Every schema change is a migration; never
   hand-edit a live DB.
7. **freqtrade lessons applied** — config-driven; strategy fully decoupled from the
   venue (via `Provider`); backtest/live parity as the core correctness property;
   event-driven design structurally prevents the lookahead bias freqtrade can only
   detect after the fact with `lookahead-analysis`.

## Backtest fill model (assumptions are explicit)
A candle is four prices and cannot say whether the high or the low came first. So:
- market order → fills at the **next** candle's open.
- limit buy → fills if `candle.low <= limit`, at the limit (or open if it gapped
  through); symmetric for sell.
- stop → triggers if `candle.high >= stop` (buy) / `candle.low <= stop` (sell).
- fee (taker/maker) + configurable slippage applied on every fill.
- **intra-bar ambiguity**: if a stop and a target both sit inside one candle, the
  model can't know which hit first and assumes the worse case. Finer-timeframe
  replay is a future improvement. Backtest P&L is a model output, never a promise.

## Reference strategy
`ratio_hop` — the coin-accumulation strategy from `ccxt/binance-trade-bot`: hold
the coin strong versus its peers, rotate `A → bridge → B` only when the post-fee
ratio beats the last-held amount of B, so unit count only ratchets up. Ships with
the **freefall guard the original lacks**: refuse to rotate into a coin down more
than `risk.freefall_pct` over the last N candles, so the reverse-greedy loop can't
accumulate a coin collapsing to zero. The worked example of the trait, and a
reminder that maximizing units ≠ maximizing value.

## Layout
```
Cargo.toml              workspace
crates/
  core/        Instrument, AssetClass, Candle, MarketEvent, Order, Signal, Position, Money(Decimal)
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
Layered; each source overrides the one above it:
1. built-in defaults (in code)
2. config file — `config/config.toml` (path overridable via `--config` or
   `BOT_CONFIG`); `config/config.example.toml` is committed and documents every field
3. environment variables, prefixed `BOT_`, nested with `__`
   (`BOT_EXCHANGE__TESTNET=true`, `BOT_STRATEGY__NAME=ratio_hop`,
   `BOT_DB__URL=sqlite://bot.db`) — this is the Docker/Heroku/CI path
4. CLI flags (`--paper`, `--live`, `--strategy`, …)

- Loader: `figment` — TOML provider + `Env::prefixed("BOT_").split("__")` —
  deserialized into one typed `Config` struct via serde with
  `#[serde(deny_unknown_fields)]` so a typo errors instead of silently defaulting.
  (`config` crate is the alternative.)
- **Secrets are the exception to the file layer (invariant 6).** `api_key` /
  `api_secret` are read ONLY from fixed env vars (`BINANCE_API_KEY`,
  `BINANCE_API_SECRET`), never placed in any TOML. The file may name which env var
  to read, never hold the value. Missing in live mode → hard error at startup.
- Validate on load, fail fast: whitelist non-empty, fee/slippage ≥ 0, live mode
  requires a non-testnet provider + the `--i-understand` gate. A bad config must
  never reach the trading loop.
- Real config is `config/*.local.toml` (gitignored) or env; only
  `config.example.toml` is committed.

## Definition of done (every change)
A change isn't finished until all of these hold — apply them per task, not at the end:
- Compiles; `cargo clippy -- -D warnings` and `cargo fmt --check` pass.
- **Tests ship in the same change.** Unit tests for new logic; a regression test
  if it touches the backtester's P&L path or the fill model. A change with no test
  is not done. Write assertions against *expected* values derived independently —
  never an assertion that just mirrors whatever the code currently returns, which
  passes vacuously and locks in bugs. For the fill model and `ratio_hop`, the
  owner supplies or confirms the expected numbers.
- **Docs in the same change.** Public items (traits, structs, `pub fn`) carry `///`
  comments stating intent and invariants, not restating the signature. Stale docs
  are a bug: update them with the code.
- If a change alters the architecture, an invariant, or a config field, update
  CLAUDE.md and `config.example.toml` in the same change.

## Commits
- One commit = one logical change. A feature, a fix, a refactor, a rename are
  separate commits even in the same session — never bundled into one.
- Commit as you go, not once at the end. When a unit is complete (code + its test
  + its docs, per Definition of done), commit it before starting the next. This,
  not a size rule, is what keeps diffs small.
- Keep mechanical changes apart from behavioral ones: a formatting-only or rename
  commit never also changes logic, so review stays about the logic.
- Each commit builds and passes tests on its own (don't break `git bisect`).
- Conventional Commit subject, imperative, ≤72 chars: `feat(storage): add trade_stats`.
  The body says why, not what the diff already shows.
- Never commit secrets, `*.local.toml`, or `data/`. Never `--force` a shared branch.
- Don't commit unless asked or the task clearly calls for it. If a change turns out
  to span several logical units, propose the split first rather than one big commit.

## Conventions
- Errors: `thiserror` in libraries, `anyhow` in the binary. No `unwrap()` outside
  tests.
- Async `tokio`; JSON `serde`; money `rust_decimal`; logs `tracing` (structured,
  no money values at info level).
- Every strategy and the fill model get unit tests. The backtester gets a
  known-input regression test so a refactor can't silently change P&L.
- Keep claims honest: if a number isn't measured (a backtest return, a latency),
  label it unverified. Mirrors the owner's first-principles preference.

## Crates
`tokio, async-trait, inventory, reqwest, tokio-tungstenite, serde/serde_json,
rust_decimal, sea-orm, sea-orm-migration, figment, thiserror, anyhow, tracing, clap, toml`.
(`barter-rs` is an option if we later prefer a ready-made framework over
hand-rolled traits; `rhai`/`wasmtime` when strategy scripting is needed.)