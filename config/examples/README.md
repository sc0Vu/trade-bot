# Example configurations

One complete, runnable config per built-in strategy:

| File | Strategy |
|---|---|
| `avwap.toml` | one coin, long above its anchored VWAP |
| `buy_and_hold.toml` | the benchmark: buy once, hold |
| `funding_carry.toml` | spot long + perpetual short for the funding (backtest only) |
| `dca.toml` | one coin, a fixed buy every period, hold |
| `sma_long_short.toml` | one coin, long above its moving average, short below on a perpetual (flat on spot) |
| `sma_trend.toml` | one coin, long above its moving average |
| `dual_ma_cross.toml` | one coin, long while the fast average is above the slow |
| `macd_cross.toml` | one coin, long while MACD is above its signal |
| `donchian_breakout.toml` | one coin, channel breakouts |
| `atr_breakout.toml` | one coin, ATR-band breakout, ATR trailing stop |
| `vol_target.toml` | one coin, sized down when volatile, up when calm |
| `volatility_breakout.toml` | one coin, buy stop above the last bar's range, one-bar hold |
| `bollinger_reversion.toml` | one coin, buy below the lower band, sell above the mean |
| `rsi_reversion.toml` | one coin, buy oversold / sell overbought |
| `momentum_rotation.toml` | several coins, hold the strongest |
| `rebalance.toml` | fixed weights (half BTC, half cash), traded back on drift |
| `ratio_hop.toml` | several coins, rotate when units ratchet up |
| `grid.toml` | one coin, spot grid |
| `tsmom_long_short.toml` | time-series momentum long and short on perpetuals (flat on spot) |
| `long_short_momentum.toml` | cross-sectional momentum, market-neutral: long the best k, short the worst k (perpetuals) |
| `turn_of_month.toml` | one instrument, held over each month's turn, cash otherwise |
| `ichimoku.toml` | one coin, long above the Ichimoku cloud with Tenkan over Kijun |
| `supertrend.toml` | one coin, long while SuperTrend says up |
| `stochastic.toml` | one coin, buy a %K/%D cross up while oversold, sell a cross down while overbought |
| `trailing_stop.toml` | buy-and-hold with a trailing exit and a re-entry off the trough |
| `pair_reversion.toml` | two coins, hold whichever is cheap relative to the other |
| `top_k_momentum.toml` | several coins, hold the best k by momentum (optional trend, volatility and schedule overlays) |
| `tsmom.toml` | time-series momentum, long-only, volatility-targeted |
| `risk_parity.toml` | several coins weighted by inverse volatility |
| `rhai.toml` | a Rhai script strategy (`strategies/sma_trend.rhai`); needs the `rhai` feature |

Venue-specific examples live in subdirectories: `etoro/` (US stocks and
ETFs on eToro: demo-account configs, backtest params, and `make
etoro-stocks-backtest`).

Run one on paper (testnet by default; set `exchange.network = "demo"` for
Demo Trading):

```
cargo run -p cli -- run --paper --config config/examples/sma_trend.toml
```

Or let the binary copy one (it carries every file here) and check it
before starting:

```
trade-bot config init --strategy sma_trend --out config/config.toml
trade-bot config check --config config/config.toml
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
