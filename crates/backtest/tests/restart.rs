//! Restart property: stopping a run and starting it again from its saved
//! state must not change what it does.
//!
//! Each case runs `backtest::check_restart` (the same check the console
//! offers for any strategy). For each built-in strategy, on its
//! regression fixture: run all bars uninterrupted. Then, for every cut point k where no order is working,
//! run bars before k, keep the strategy's last saved state, build a fresh
//! strategy, restore it, and run the remaining bars after a warm-up (the
//! earlier bars as Backfill, as the live feed does), against the same
//! broker (the books survive a restart). The two runs' fills together must
//! equal the uninterrupted run's.
//!
//! Only cuts with no working order are compared: a live restart's
//! reconcile cancels working orders, which genuinely changes what follows
//! (unit tests cover how strategies restore from that). `grid` always has
//! orders working, so it has no case here: `tests/grid.rs` checks that a
//! restart after the reconcile rebuilds the same book. Nor has
//! `volatility_breakout`: flat it always has a buy stop working, long its
//! exit, so no cut is idle. It is stateless (books + last bar), and its
//! unit tests cover deciding from whatever the reconcile left.

mod common;

use bot_core::InstrumentId;
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

struct Case {
    strategy: &'static str,
    fixture: &'static str,
    coins: &'static [&'static str],
    bars: i64,
    params: &'static str,
}

/// `backtest::check_restart` at every cut of the fixture's bars: it must
/// pass, and restart at least once while holding a position.
async fn run_case(c: &Case) {
    let ids: Vec<InstrumentId> = c.coins.iter().map(|s| s.parse().unwrap()).collect();
    let dir = backtest::DataDir::new(format!(
        "{}/tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        c.fixture
    ));
    let cached = backtest::BacktestProvider::from_cache(
        &dir,
        &ids,
        bot_core::Interval::H1,
        bot_core::Timestamp::from_millis(0),
        bot_core::Timestamp::from_millis(c.bars * 3_600_000),
        backtest::PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: bot_core::Notional::new(dec!(10000)),
        },
    )
    .unwrap();
    // Costs as live; cash ample (the property is about state, not money).
    let paper = backtest::PaperConfig {
        fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
        slippage: BpsSlippage::new(dec!(10)).unwrap(),
        initial_cash: bot_core::Notional::new(dec!(1000000000)),
    };
    let config = EngineConfig {
        whitelist: ids,
        history_len: 20,
        ..Default::default()
    };
    let params = Params::from_toml(c.params).unwrap();
    let r = backtest::check_restart(
        cached.instruments(),
        cached.candles(),
        paper,
        &|_| strategy::build(c.strategy, &params),
        &params,
        &config,
        c.bars as usize,
    )
    .await
    .unwrap_or_else(|e| panic!("{}: {e}", c.strategy));
    assert!(
        r.while_holding > 0,
        "{}: never restarted while holding a position",
        c.strategy
    );
}

const BTC: &[&str] = &["binance:BTCUSDT"];
const BTC_ETH: &[&str] = &["binance:BTCUSDT", "binance:ETHUSDT"];
const BTC_ETH_SOL: &[&str] = &["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"];

#[tokio::test]
async fn every_strategy_survives_a_restart() {
    let cases = [
        Case {
            strategy: "buy_and_hold",
            fixture: "sma_trend",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                quote_amount = "1009.08"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "dca",
            fixture: "sma_trend",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                quote_amount = "300"
                every = "hour"
                max_buys = 3
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "sma_trend",
            fixture: "sma_trend",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                sma_period = 3
                quote_amount = "1009.08"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "sma_long_short",
            fixture: "sma_trend",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                sma_period = 3
                notional = "1000"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "avwap",
            fixture: "sma_trend",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                anchor = "day"
                quote_amount = "1009.08"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "dual_ma_cross",
            fixture: "dual_ma_cross",
            coins: BTC,
            bars: 9,
            params: r#"coin = "binance:BTCUSDT"
                fast_period = 2
                slow_period = 4
                quote_amount = "1081.08"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "donchian_breakout",
            fixture: "donchian_breakout",
            coins: BTC,
            bars: 8,
            params: r#"coin = "binance:BTCUSDT"
                channel_period = 3
                quote_amount = "972.99"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "atr_breakout",
            fixture: "atr_breakout",
            coins: BTC,
            bars: 9,
            params: r#"coin = "binance:BTCUSDT"
                period = 3
                entry_mult = "0.5"
                exit_mult = "1"
                quote_amount = "954.99"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "rebalance",
            fixture: "rebalance",
            coins: BTC,
            bars: 8,
            params: r#"weights = { "binance:BTCUSDT" = "0.5" }
                capital = "2000"
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "rebalance",
            fixture: "momentum_rotation",
            coins: BTC_ETH,
            bars: 8,
            params: r#"weights = { "binance:BTCUSDT" = "0.4", "binance:ETHUSDT" = "0.4" }
                capital = "2000"
                band = "0.02"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "risk_parity",
            fixture: "risk_parity",
            coins: BTC_ETH,
            bars: 10,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                capital = "10000"
                vol_period = 2
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "tsmom",
            fixture: "tsmom",
            coins: BTC_ETH,
            bars: 12,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                capital = "10000"
                lookback = 3
                target_vol = "1.5"
                vol_period = 3
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        // On spot (the harness's broker) its short side is flat: the
        // long/flat path is what a restart can cut here.
        Case {
            strategy: "tsmom_long_short",
            fixture: "tsmom",
            coins: BTC_ETH,
            bars: 12,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                capital = "10000"
                lookback = 3
                vol_period = 3
                target_vol = "1.5"
                max_weight = "1"
                band = "0.2"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "trailing_stop",
            fixture: "trailing_stop",
            coins: BTC,
            bars: 12,
            params: r#"coin = "binance:BTCUSDT"
                quote_amount = "1000"
                trail_pct = "0.1"
                reentry_pct = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "pair_reversion",
            fixture: "pair_reversion",
            coins: BTC_ETH,
            bars: 12,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                capital = "10000"
                period = 4
                entry_z = "1"
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "top_k_momentum",
            fixture: "top_k_momentum",
            coins: BTC_ETH_SOL,
            bars: 12,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
                k = 2
                lookback = 2
                capital = "9000"
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        // 760 hourly bars, 1970-01-01 to Feb 1 16:00: the window opens
        // with Jan 31's bars and closes at February's 3rd bar.
        Case {
            strategy: "turn_of_month",
            fixture: "turn_of_month",
            coins: BTC,
            bars: 760,
            params: r#"coin = "binance:BTCUSDT"
                enter_days = 1
                hold_bars = 3
                quote_amount = "1000"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        // On spot (the harness's broker) the short leg is flat: ranking
        // and the long leg are what a restart can cut here.
        Case {
            strategy: "long_short_momentum",
            fixture: "top_k_momentum",
            coins: BTC_ETH_SOL,
            bars: 12,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
                k = 1
                lookback = 2
                rebalance_bars = 2
                capital = "6000"
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        // Every overlay: the saved rebalance time drives the schedule and
        // the stop's window.
        Case {
            strategy: "top_k_momentum",
            fixture: "top_k_momentum",
            coins: BTC_ETH_SOL,
            bars: 12,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
                k = 2
                lookbacks = [1, 2]
                min_momentum = "-0.5"
                trend_period = 2
                weighting = "inverse_vol"
                vol_period = 2
                rebalance_every = "day"
                trail_pct = "0.01"
                capital = "9000"
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "stochastic",
            fixture: "indicators",
            coins: BTC,
            bars: 30,
            params: r#"coin = "binance:BTCUSDT"
                k_period = 3
                d_period = 2
                oversold = "30"
                overbought = "70"
                quote_amount = "1000"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "supertrend",
            fixture: "indicators",
            coins: BTC,
            bars: 30,
            params: r#"coin = "binance:BTCUSDT"
                period = 3
                mult = "1.5"
                window = 8
                quote_amount = "1000"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "ichimoku",
            fixture: "indicators",
            coins: BTC,
            bars: 30,
            params: r#"coin = "binance:BTCUSDT"
                tenkan = 2
                kijun = 3
                senkou_b = 4
                displacement = 2
                quote_amount = "1000"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "vol_target",
            fixture: "vol_target",
            coins: BTC,
            bars: 12,
            params: r#"coin = "binance:BTCUSDT"
                capital = "2000"
                target_vol = "0.8"
                vol_period = 3
                max_weight = "0.9"
                band = "0.05"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "bollinger_reversion",
            fixture: "rsi_reversion",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                period = 3
                k = "1"
                quote_amount = "850.9"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "macd_cross",
            fixture: "macd_cross",
            coins: BTC,
            bars: 12,
            params: r#"coin = "binance:BTCUSDT"
                fast_period = 1
                slow_period = 2
                signal_period = 2
                quote_amount = "1061.1"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "rsi_reversion",
            fixture: "rsi_reversion",
            coins: BTC,
            bars: 7,
            params: r#"coin = "binance:BTCUSDT"
                rsi_period = 2
                oversold = "30"
                overbought = "70"
                quote_amount = "850.9"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "momentum_rotation",
            fixture: "momentum_rotation",
            coins: BTC_ETH,
            bars: 8,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                lookback = 2
                quote_amount = "990.99"
                fee = "0.001"
                slippage_bps = "10""#,
        },
        Case {
            strategy: "ratio_hop",
            fixture: "ratio_hop",
            coins: BTC_ETH,
            bars: 7,
            params: r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                quote_amount = "1000"
                fee = "0.001"
                slippage_bps = "10"
                freefall_pct = "0.5"
                freefall_candles = 2
                order_type = "market""#,
        },
    ];
    for c in &cases {
        run_case(c).await;
    }
}

/// The shipped `sma_trend.rhai` script restarts like the built-in: its
/// `this` state travels through the same save / restore path.
#[cfg(feature = "rhai")]
#[tokio::test]
async fn rhai_script_survives_a_restart() {
    let params = format!(
        "script = {:?}\ncoin = \"binance:BTCUSDT\"\nsma_period = 3\n\
         quote_amount = \"1009.08\"\nfee = \"0.001\"\nslippage_bps = \"10\"",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../strategies/sma_trend.rhai"
        )
    );
    run_case(&Case {
        strategy: "rhai",
        fixture: "sma_trend",
        coins: BTC,
        bars: 7,
        params: Box::leak(params.into_boxed_str()),
    })
    .await;
}
