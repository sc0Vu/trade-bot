# eToro examples

Stocks and ETFs on eToro, bought as the real asset (long only, no
leverage). Every eToro call, data included, needs your keys in the
environment: `ETORO_PUBLIC_KEY` (the application's API key) and
`ETORO_PRIVATE_KEY` (your account's user key; a demo key trades your demo
account).

## Backtests

```
make etoro-stocks-data                                   # daily bars since mid-2023
make etoro-stocks-backtest FROM=2024-01-01 TO=2026-11-01 # each example vs buy-and-hold SPY
```

Or one at a time with the generic targets (`params/` holds plain params
tables for `backtest --params`):

```
make fetch-data VENUE=etoro SYMBOL=AAPL.RTH INTERVAL=1d FROM=2023-06-01
make backtest VENUE=etoro STRATEGY=sma_trend SYMBOL=AAPL.RTH INTERVAL=1d \
     FROM=2024-01-01 TO=2026-11-01 SLIPPAGE_BPS=5 FEE=0.0002 \
     INITIAL_CASH=10000 HISTORY_LEN=100 PARAMS=config/examples/etoro/params/sma_trend-aapl.toml
```

Results over 2024-01-01 to 2026-11-01 ($10,000, 5 bps, fee 0.0002; model
output, measured 2026-11-18, not a promise):

| Example | Final equity | Max drawdown |
|---|---|---|
| `buy_and_hold-spy` (benchmark) | 15,943 | 18.8% |
| `sma_trend-aapl` | 12,662 | 17.8% |
| `top_k_momentum-stocks` | 11,375 | 46.3% |

Neither beat holding SPY over this window.

Things to know:

- `.RTH` instruments trade regular US hours only and their history starts
  in mid-2023. The plain symbols (`AAPL`, `SPY`) trade eToro's extended
  hours and have daily bars from August 2018 (hourly from 2011 for AAPL):
  use them for longer backtests.
- eToro charges a fixed $1 per stock order (seen on the demo account). The
  backtest models fees as a rate, so set `--fee` to $1 / your order size:
  `0.001` for $1,000 orders, `0.01` for $100 ones. Small orders are
  expensive.
- Sharpe is annualized as if markets never closed (365 days a year), which
  overstates it for stocks by about √(365/252) ≈ 1.2.
- A series is cut as delisted only after 120 hours without a bar, so
  weekends and holidays don't end it.

## Runs on the demo account

```
cargo run -p cli -- run --paper --config config/examples/etoro/sma_trend-aapl.toml
```

`sma_trend-aapl.toml` and `top_k_momentum-stocks.toml` trade your demo
account (`network = "demo"`). Real money needs `mode = "live"`,
`network = "live"`, `--i-understand` on the command line, and for US
stocks `[etoro] w8ben_signed = true` once your W-8BEN is on file.
