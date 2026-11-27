//! No-lookahead property (invariant 2), end to end: changing the future
//! must not change the past, for every built-in strategy.
//!
//! Each case runs `backtest::check_lookahead` (the same check the console
//! offers for any strategy) at every cut point t: all candles after t are
//! replaced (BTC halved, ETH doubled), and every decision (order accepted,
//! signal refused) and fill up to t's close must be identical. A strategy,
//! engine or broker that peeked ahead would break this.

use backtest::{BacktestProvider, DataDir, PaperConfig};
use bot_core::{InstrumentId, Interval, Notional, Timestamp};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

/// `backtest::check_lookahead` over every cut of a fixture's `bars` bars
/// for `ids`, with costs (0.1% fee, 10 bps): it must pass, and not
/// vacuously (the rewrite must change some later fill).
async fn property(fixture: &str, ids: &[&str], bars: i64, strategy: &str, params: &str) {
    let ids: Vec<InstrumentId> = ids.iter().map(|s| s.parse().unwrap()).collect();
    let dir = DataDir::new(format!(
        "{}/tests/fixtures/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    ));
    let cached = BacktestProvider::from_cache(
        &dir,
        &ids,
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(bars * HOUR),
        PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(10000)),
        },
    )
    .unwrap();
    let paper = PaperConfig {
        fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
        slippage: BpsSlippage::new(dec!(10)).unwrap(),
        initial_cash: Notional::new(dec!(10000)),
    };
    let params = Params::from_toml(params).unwrap();
    let config = EngineConfig {
        whitelist: ids,
        history_len: 10,
        ..Default::default()
    };
    let r = backtest::check_lookahead(
        cached.instruments(),
        cached.candles(),
        paper,
        &|_| strategy::build(strategy, &params),
        &params,
        &config,
        bars as usize,
    )
    .await
    .unwrap_or_else(|e| panic!("{strategy}: {e}"));
    assert!(
        r.future_mattered,
        "rewriting the future never changed any {strategy} fill"
    );
}

const BTC_ETH_SOL: &[&str] = &["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"];
const BTC_ETH: &[&str] = &["binance:BTCUSDT", "binance:ETHUSDT"];

async fn single_coin_property(fixture: &str, bars: i64, strategy: &str, params: &str) {
    property(fixture, &["binance:BTCUSDT"], bars, strategy, params).await;
}

#[tokio::test]
async fn ratio_hop_never_lets_the_future_change_the_past() {
    property(
        "ratio_hop",
        BTC_ETH,
        7,
        "ratio_hop",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10"
           freefall_pct = "0.5"
           freefall_candles = 2
           order_type = "market""#,
    )
    .await;
}

#[tokio::test]
async fn sma_trend_never_lets_the_future_change_the_past() {
    single_coin_property(
        "sma_trend",
        7,
        "sma_trend",
        r#"coin = "binance:BTCUSDT"
           sma_period = 3
           quote_amount = "1009.08"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn buy_and_hold_never_lets_the_future_change_the_past() {
    single_coin_property(
        "sma_trend",
        7,
        "buy_and_hold",
        r#"coin = "binance:BTCUSDT"
           quote_amount = "1009.08"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn avwap_never_lets_the_future_change_the_past() {
    single_coin_property(
        "sma_trend",
        7,
        "avwap",
        r#"coin = "binance:BTCUSDT"
           anchor = "day"
           quote_amount = "1009.08"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn grid_never_lets_the_future_change_the_past() {
    single_coin_property(
        "sma_trend",
        7,
        "grid",
        r#"coin = "binance:BTCUSDT"
           lower = "95"
           upper = "115"
           grids = 4
           spacing = "arithmetic"
           quote_per_grid = "100"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn momentum_rotation_never_lets_the_future_change_the_past() {
    property(
        "momentum_rotation",
        BTC_ETH,
        8,
        "momentum_rotation",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           lookback = 2
           quote_amount = "990.99"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn rsi_reversion_never_lets_the_future_change_the_past() {
    single_coin_property(
        "rsi_reversion",
        7,
        "rsi_reversion",
        r#"coin = "binance:BTCUSDT"
           rsi_period = 2
           oversold = "30"
           overbought = "70"
           quote_amount = "850.9"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn dual_ma_cross_never_lets_the_future_change_the_past() {
    single_coin_property(
        "dual_ma_cross",
        9,
        "dual_ma_cross",
        r#"coin = "binance:BTCUSDT"
           fast_period = 2
           slow_period = 4
           quote_amount = "1081.08"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn donchian_breakout_never_lets_the_future_change_the_past() {
    single_coin_property(
        "donchian_breakout",
        8,
        "donchian_breakout",
        r#"coin = "binance:BTCUSDT"
           channel_period = 3
           quote_amount = "972.99"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn atr_breakout_never_lets_the_future_change_the_past() {
    single_coin_property(
        "atr_breakout",
        9,
        "atr_breakout",
        r#"coin = "binance:BTCUSDT"
           period = 3
           entry_mult = "0.5"
           exit_mult = "1"
           quote_amount = "954.99"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn volatility_breakout_never_lets_the_future_change_the_past() {
    single_coin_property(
        "volatility_breakout",
        8,
        "volatility_breakout",
        r#"coin = "binance:BTCUSDT"
           k = "0.5"
           quote_amount = "1010"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn rebalance_never_lets_the_future_change_the_past() {
    single_coin_property(
        "rebalance",
        8,
        "rebalance",
        r#"weights = { "binance:BTCUSDT" = "0.5" }
           capital = "2000"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn vol_target_never_lets_the_future_change_the_past() {
    single_coin_property(
        "vol_target",
        12,
        "vol_target",
        r#"coin = "binance:BTCUSDT"
           capital = "2000"
           target_vol = "0.8"
           vol_period = 3
           max_weight = "0.9"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn bollinger_reversion_never_lets_the_future_change_the_past() {
    single_coin_property(
        "rsi_reversion",
        7,
        "bollinger_reversion",
        r#"coin = "binance:BTCUSDT"
           period = 3
           k = "1"
           quote_amount = "850.9"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn macd_cross_never_lets_the_future_change_the_past() {
    single_coin_property(
        "macd_cross",
        12,
        "macd_cross",
        r#"coin = "binance:BTCUSDT"
           fast_period = 1
           slow_period = 2
           signal_period = 2
           quote_amount = "1061.1"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

/// Spot fixture: the long leg trades, the short leg is flat; the ranking
/// reads every coin's history at the deciding close only.
#[tokio::test]
async fn long_short_momentum_never_lets_the_future_change_the_past() {
    property(
        "top_k_momentum",
        BTC_ETH_SOL,
        12,
        "long_short_momentum",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
           k = 1
           lookback = 2
           capital = "6000"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

// The rest of the built-ins, with their restart cases' params
// (`tests/restart.rs`). Not here: funding_carry (decides on
// `MarketEvent::Funding`, which this candle-only check can't rewrite) and
// rhai (its reference script's fills equal sma_trend's,
// `tests/rhai_script.rs`).

const BTC: &[&str] = &["binance:BTCUSDT"];

#[tokio::test]
async fn dca_never_lets_the_future_change_the_past() {
    single_coin_property(
        "sma_trend",
        7,
        "dca",
        r#"coin = "binance:BTCUSDT"
           quote_amount = "300"
           every = "hour"
           max_buys = 3
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

/// On spot its short side is flat; the long/flat decisions are checked.
#[tokio::test]
async fn sma_long_short_never_lets_the_future_change_the_past() {
    single_coin_property(
        "sma_trend",
        7,
        "sma_long_short",
        r#"coin = "binance:BTCUSDT"
           sma_period = 3
           notional = "1000"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn risk_parity_never_lets_the_future_change_the_past() {
    property(
        "risk_parity",
        BTC_ETH,
        10,
        "risk_parity",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           capital = "10000"
           vol_period = 2
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn tsmom_never_lets_the_future_change_the_past() {
    property(
        "tsmom",
        BTC_ETH,
        12,
        "tsmom",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           capital = "10000"
           lookback = 3
           target_vol = "1.5"
           vol_period = 3
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

/// On spot its short side is flat; each coin's own decision is checked.
#[tokio::test]
async fn tsmom_long_short_never_lets_the_future_change_the_past() {
    property(
        "tsmom",
        BTC_ETH,
        12,
        "tsmom_long_short",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           capital = "10000"
           lookback = 3
           vol_period = 3
           target_vol = "1.5"
           max_weight = "1"
           band = "0.2"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn trailing_stop_never_lets_the_future_change_the_past() {
    single_coin_property(
        "trailing_stop",
        12,
        "trailing_stop",
        r#"coin = "binance:BTCUSDT"
           quote_amount = "1000"
           trail_pct = "0.1"
           reentry_pct = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn pair_reversion_never_lets_the_future_change_the_past() {
    property(
        "pair_reversion",
        BTC_ETH,
        12,
        "pair_reversion",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           capital = "10000"
           period = 4
           entry_z = "1"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn top_k_momentum_never_lets_the_future_change_the_past() {
    property(
        "top_k_momentum",
        BTC_ETH_SOL,
        12,
        "top_k_momentum",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
           k = 2
           lookback = 2
           capital = "9000"
           band = "0.05"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

/// Every overlay: the trend filter, inverse-vol weights, the calendar
/// schedule and the trailing stop all read history up to the cut only.
#[tokio::test]
async fn top_k_momentum_overlays_never_let_the_future_change_the_past() {
    property(
        "top_k_momentum",
        BTC_ETH_SOL,
        12,
        "top_k_momentum",
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT", "binance:SOLUSDT"]
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
    )
    .await;
}

/// 760 hourly bars over Jan 31 → Feb 1 1970: the month's turn is the only
/// window, and the calendar comes from each bar's own close time.
#[tokio::test]
async fn turn_of_month_never_lets_the_future_change_the_past() {
    property(
        "turn_of_month",
        BTC,
        760,
        "turn_of_month",
        r#"coin = "binance:BTCUSDT"
           enter_days = 1
           hold_bars = 3
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn stochastic_never_lets_the_future_change_the_past() {
    single_coin_property(
        "indicators",
        30,
        "stochastic",
        r#"coin = "binance:BTCUSDT"
           k_period = 3
           d_period = 2
           oversold = "30"
           overbought = "70"
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

#[tokio::test]
async fn supertrend_never_lets_the_future_change_the_past() {
    single_coin_property(
        "indicators",
        30,
        "supertrend",
        r#"coin = "binance:BTCUSDT"
           period = 3
           mult = "1.5"
           window = 8
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}

/// Its spans are displaced into the past (computed `displacement` bars
/// back), never projected forward as charts draw them.
#[tokio::test]
async fn ichimoku_never_lets_the_future_change_the_past() {
    single_coin_property(
        "indicators",
        30,
        "ichimoku",
        r#"coin = "binance:BTCUSDT"
           tenkan = 2
           kijun = 3
           senkou_b = 4
           displacement = 2
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .await;
}
