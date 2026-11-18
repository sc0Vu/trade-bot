# Writing a script strategy

A strategy can be a [Rhai](https://rhai.rs) script instead of Rust: no
compiler, no rebuild. It runs in the same engine as the built-ins, so a
script's backtest and its live run make the same decisions. This page is
the whole script API; the reference implementation is
`crates/strategy/src/script/rhai.rs` and a complete example is
`strategies/sma_trend.rhai` (it makes exactly the built-in `sma_trend`'s
fills).

Script support is the cargo feature `rhai`. Release binaries and the
container have it; from source, build with `--features rhai`.

## Running one

The fastest loop is the console: `make console`, open
<http://127.0.0.1:9186/>, pick *+ New script*, edit, then *Check*,
*Check lookahead* and *Run backtest*. From a config:

```toml
[strategy]
name = "rhai"
[strategy.params]
script = "strategies/my_trend.rhai"   # relative to the working directory
coin = "binance:BTCUSDT"              # everything else is the script's own params
quote_amount = "100"
fee = "0.001"
slippage_bps = "5"
```

## How a script is called

- The engine calls the script's hooks; nothing at the top level of the
  file runs.
- A strategy decides on a **closed** candle and its market orders fill at
  the **next** candle's open (in a backtest, by the stated fill model;
  live, by the venue). It can't see anything after the current candle.
- Scripts get candles through `on_candle` only. Funding, delisting and
  trading-halt events are handled by the engine and not passed to scripts
  (the engine closes a coin before its delisting whatever the script does).
- Hooks return signals; the engine sends them through the same risk checks
  as every strategy (whitelist, lot and tick rounding, venue filters, price
  band). A refused signal comes back through `on_rejected`.

## Numbers

There are no floats. `1000` and `-3` are integers; any number with a
decimal point (`0.001`, `1e-3`) is an exact decimal. They mix in
arithmetic. Every price, quantity and amount the host hands over is a
decimal, and every money argument accepts an integer or a decimal.

Money in params is a TOML string (`fee = "0.001"`); turn it into a number
with `parse_decimal(p.fee)`. A TOML float in params is refused.

Available math: `+ - * / % **`, `abs`, `min`, `max`, `round(dp)`,
`floor`, `ceiling`, `sqrt`, `to_decimal`, `to_int`, `parse_decimal`,
`parse_int`. Use the instrument's own functions (below) for anything that
touches lots, ticks or notional; never round by hand.

## Hooks

All optional. Each `on_*` hook returns `()`, one signal, or an array of
signals.

| Hook | When |
|---|---|
| `check_params(params)` | At build, before anything trades. `throw "reason"` refuses the config. |
| `on_start(ctx)` | Once, after the books are loaded. |
| `on_candle(ctx, candle)` | Every closed candle of every whitelisted instrument. |
| `on_fill(ctx, fill)` | Each fill of one of its orders, before that candle joins the history. |
| `on_order(ctx, order)` | An order it sent was accepted (learn its id to cancel it later). |
| `on_rejected(ctx, signal, reason)` | A signal was refused (risk or venue). |
| `restore(saved, ctx)` | On restart, with the state the last run saved: return `()` to take it, or a string saying why it contradicts the books (then the script starts fresh). No `restore` = always fresh. |

## `ctx`

A read-only snapshot, fresh for each hook call.

| Member | Gives |
|---|---|
| `ctx.now` | Current event time, ms (`()` before the first event) |
| `ctx.params` | The params map |
| `ctx.instruments` | Ids in the universe |
| `ctx.instrument(id)` | Instrument, or `()` outside the universe |
| `ctx.position(id)` | Position (flat if none) |
| `ctx.open_orders(id)` | Its working orders on `id` |
| `ctx.cancelling_orders(id)` | Orders it cancelled that may still fill |
| `ctx.history(id, n)` | The newest `n` closed candles, oldest first, ending at the current one |
| `ctx.history(id)` | All of them (costs time in proportion to `history_len`: prefer the `n` form) |
| `ctx.last_candle(id)` | The latest candle, or `()` |

Ids are strings (`"binance:BTCUSDT"`); `instrument_id(s)` normalizes one
for comparing with `candle.instrument`. Sides are `"buy"` / `"sell"`;
times are ms integers.

## Types

- **Candle**: `instrument open_time close_time open high low close volume quote_volume`
- **Instrument**: `id base quote tick_size lot_size min_qty min_notional`, and
  - `round_qty(q)`: down to the lot
  - `round_price(p, side)`: to the tick, toward the passive side
  - `market_price(reference, side)`: a reference price rounded against you
  - `notional(q, p)`, `qty_for(notional, p)` (`()` if it fails the filters)
  - `order_ok(q, p)`, `market_ok(q, reference, side)`: would the venue take it
- **Position**: `instrument qty cost_basis realized_pnl is_flat` (`qty` is
  negative for a short on a perpetual)
- **Order**: `id instrument side kind` (`market` `limit` `stop_market`
  `stop_limit`) `qty quote price stop created_at` (`()` where not set)
- **Fill**: `order_id instrument side qty price liquidity ts fee_kind`
  (`base` `quote` `other`) `fee_amount fee_asset fee_value`
- **Signal** (in `on_rejected`): `action` (`submit` `reduce` `cancel`)
  `instrument side order_id`

## Signals

| Function | Order |
|---|---|
| `market_buy(id, qty)` / `market_sell(id, qty)` | Market, base quantity |
| `market_buy_quote(id, quote)` | Market buy spending at most `quote`, fees included |
| `limit_buy(id, qty, price)` / `limit_sell(..)` | Limit, GTC; a 4th argument `"ioc"` / `"fok"` / `"gtc"` sets time in force |
| `stop_buy(id, qty, stop)` / `stop_sell(..)` | Stop-market |
| `stop_limit_buy(id, qty, stop, limit)` / `stop_limit_sell(..)` | Stop-limit |
| `market_reduce(id, side, qty)` | Reduce-only market: never opens or flips a position |
| `cancel(order_id)` | Cancel one of its working orders |

On eToro, IOC/FOK limits and stops are refused (the venue has no such
order).

## State

`this` is one map, empty at build, kept across hooks:
`this.held = true;`. It is saved after every hook, so it may hold only
`()`, booleans, integers, decimals, strings, arrays and maps; anything else
stops the run. Saved state is handed back only to the exact same script
(by SHA-256): an edited script starts fresh.

Prefer deriving decisions from `ctx.position` and `ctx.history` over
remembering them in `this`: positions and history are rebuilt on a
restart, and a script that needs nothing else restarts cleanly.

## Sandbox

No files, network, clock, randomness, `eval` or `import`; `print` and
`debug` go to the log. Each hook call may run at most 1,000,000
operations and nest 64 calls deep; strings, arrays and maps have size
caps. Any error (a `throw`, a type mismatch, the budget) stops the run
before anything the failing call returned is sent.

## A complete example

```rust
// Long while the close is above its 20-bar average, cash below.

fn check_params(p) {
    instrument_id(p.coin);
    if parse_decimal(p.quote_amount) <= 0 { throw "quote_amount must be positive"; }
    parse_decimal(p.fee);
    parse_decimal(p.slippage_bps);
}

fn on_candle(ctx, c) {
    let p = ctx.params;
    let coin = instrument_id(p.coin);
    // Only this coin's candles, and never decide twice while an order works.
    if c.instrument != coin || ctx.open_orders(coin).len() > 0 { return; }

    let n = 20;
    let hist = ctx.history(coin, n);
    if hist.len() < n { return; }
    let sum = 0;
    for b in hist { sum += b.close; }
    let sma = sum / n;

    // "Long" = holding something a sale would actually pass the filters
    // for; a fee's dust counts as cash, or the script could never buy again.
    let inst = ctx.instrument(coin);
    let units = inst.round_qty(ctx.position(coin).qty);
    let long = inst.market_ok(units, c.close, "sell");

    if !long && c.close > sma { return market_buy_quote(coin, parse_decimal(p.quote_amount)); }
    if long && c.close < sma { return market_sell(coin, units); }
}

fn restore(saved, ctx) {}   // stateless: any saved state is fine
```

## Before trusting a script

1. **Check**: it builds and its `check_params` accepts the params.
2. **Check lookahead**: rewriting the future at 10 cut points must not
   change any decision or fill before them. A vacuous pass (the strategy
   never traded) proves nothing.
3. **Check restart**: stopping and restarting from the saved state at up
   to 10 points must not change the fills. A script that keeps decisions
   in `this` needs a `restore` that takes them back (or derives them from
   the books). A pass with no restart while holding a position tested
   little.
4. **Backtest against buy-and-hold**, after realistic fee and slippage, on
   data with no warnings (the backtest flags bars that moved over 30%: an
   unadjusted split or a bad print). A strategy that can't beat holding
   has no edge there.
5. **Paper run** before real money; real money needs the three locks.
