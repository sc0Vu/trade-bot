# Adding a built-in strategy (Rust)

Most people should write a script instead: no Rust, no rebuild, and the
console checks and backtests it ([scripting.md](scripting.md)). A built-in
is for strategies that need full speed, every event type (funding,
delistings, halts), or code shared with other built-ins. Built-ins run in
backtests, paper and live like the 28 that ship.

## 1. The file

Create `crates/strategy/src/builtin/<name>.rs` and add `pub mod <name>;` to
`builtin/mod.rs`. A small complete example to copy is `stochastic.rs`
(single coin, long or flat, using the shared `long_flat` helpers); a
multi-coin one on the shared `portfolio` code is `top_k_momentum.rs`.

```rust
const NAME: &str = "my_strategy";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]          // a typo in a config is an error, not a default
struct MyParams {
    coin: InstrumentId,
    period: usize,
    quote_amount: Notional,            // money: Notional / Price / Qty, read from strings
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,                      // rates: exact_decimal (a TOML float is refused)
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

pub struct MyStrategy { p: MyParams }

impl MyStrategy {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: MyParams = params.parse(NAME)?;
        // Check every param here: a bad config fails at start, not mid-run.
        Ok(Box::new(Self { p }))
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: MyStrategy::build }
}
```

Registration is that `inventory::submit!`: there is no central list.

## 2. The rules a strategy follows

- **Its only window is `Context`**: positions, history (closed candles up
  to the current one), open and cancelling orders, the schedule of
  delistings and halts, params, `submit`. Never a broker, network or file.
- **Decide on closed candles.** `on_candle` (or `on_event` for other
  events) returns signals; market orders fill at the next open.
- **Money is exact.** `Decimal` in unit types; there is no `Qty * Price`:
  convert with the instrument (`notional`, `qty_for`, `round_qty`,
  `round_price`, `check_market`).
- **Don't decide while an order on the coin works** (`ctx.open_orders`,
  `ctx.cancelling_orders`), or a decision doubles it.
- **Dust is cash**: a holding a sale wouldn't pass the venue's filters for
  (`long_flat::long_units`), or the strategy never buys again.
- **Exits that only reduce are `Signal::Reduce`**: they never open or flip,
  and need no margin, so a refused order can't leave a position stuck.
  A flip is a reduce-only close now, the new side at the next decision.
- **Sizes come from params or the books, never the wallet**: the strategy
  can't see balances. Say in the docs what capital a config needs.

## 3. Saved state

After every event the engine saves `save_state()`; a restart hands it to
`restore_state()` after `on_start`. If decisions follow from positions and
history alone, use `super::stateless_save()` / `stateless_restore()`.
Otherwise save what's needed (money as strings, a `"v": STATE_VERSION`)
and refuse state that contradicts the books.

## 4. Tests (a change isn't done without them)

- **Unit tests** in the file, with expected values worked out by hand in
  a comment, never copied from what the code returned.
- **An example config** `config/examples/<name>.toml`, a runnable config
  with textbook params. A test fails if a registered strategy has none, and
  loads and builds every one.
- **Lookahead**: a case in `crates/backtest/tests/lookahead.rs` (fixture,
  params). It runs `backtest::check_lookahead`, which fails if any
  decision or fill up to a cut changes when only later candles do.
- **Restart**: a case in `crates/backtest/tests/restart.rs`. It runs
  `backtest::check_restart`: stopped and restarted from saved state at
  every idle point, the fills must not change. A strategy that always has
  an order working has no idle point; say why in the file and test its
  restore some other way (see `grid`).
- `make check` (format, clippy, docs, tests) passes.

Neither the lookahead nor the restart case is enforced by a test yet:
leaving one out is easy, so check the two files.

## 5. Docs

- Module docs at the top of the file: what it bets on, why that might hold
  after fees and slippage, how it fails, and "edge unverified" unless
  measured.
- A row in the README's strategy table and in `config/examples/README.md`.
- A row in the website's strategy table (`site/index.html`).
- `CLAUDE.md`: the built-in list under Layout, the state list, and a short
  description.

## 6. Before trusting it

Backtest it against `buy_and_hold` on the same coin, dates and costs
(`make backtest`, or the console). A strategy that can't beat holding after
costs has no edge there. Then paper, then real money behind the three
locks.
