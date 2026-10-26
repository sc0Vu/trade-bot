# Example configurations

One complete, runnable config per built-in strategy:

| File | Strategy |
|---|---|
| `avwap.toml` | one coin, long above its anchored VWAP |
| `buy_and_hold.toml` | the benchmark: buy once, hold |
| `sma_trend.toml` | one coin, long above its moving average |
| `dual_ma_cross.toml` | one coin, long while the fast average is above the slow |
| `donchian_breakout.toml` | one coin, channel breakouts |
| `rsi_reversion.toml` | one coin, buy oversold / sell overbought |
| `momentum_rotation.toml` | several coins, hold the strongest |
| `ratio_hop.toml` | several coins, rotate when units ratchet up |
| `grid.toml` | one coin, spot grid |

Run one on paper (testnet by default; set `exchange.network = "demo"` for
Demo Trading):

```
cargo run -p cli -- run --paper --config config/examples/sma_trend.toml
```

API keys never go in these files: set `BINANCE_API_KEY` and
`BINANCE_API_SECRET` in the environment. Copy a file to
`config/config.toml` (gitignored) or `config/*.local.toml` to make it yours.

The params are textbook defaults, not tuned, and no strategy here has a
verified edge: backtest one on the coins and interval you mean to trade
first. `backtest --params` takes a plain params table, i.e. what sits under
`[strategy.params]` here.

Each strategy's module (`crates/strategy/src/builtin/`) documents its
params. A test loads every file here and builds its strategy, and fails if
a registered strategy has no example.
