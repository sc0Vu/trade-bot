//! No-lookahead property (invariant 2), end to end: changing the future
//! must not change the past, for every built-in strategy.
//!
//! For every cut point t, all candles after t are replaced (BTC halved,
//! ETH doubled) and ratio_hop runs on both feeds. Every fill stamped at or
//! before t's close must be identical: those fills happen on candles up to
//! t, from decisions made on candles before them, so nothing later may
//! influence them. A strategy, engine or broker that peeked ahead would
//! break this.

use backtest::{BacktestProvider, DataDir, PaperConfig};
use bot_core::{Candle, CandleSpec, Decimal, Fill, InstrumentId, Interval, Notional, Timestamp};
use engine::{EngineConfig, RunReport};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;
const BARS: i64 = 7;

fn ids() -> Vec<InstrumentId> {
    vec![
        "binance:BTCUSDT".parse().unwrap(),
        "binance:ETHUSDT".parse().unwrap(),
    ]
}

fn fixture() -> (Vec<bot_core::Instrument>, Vec<Candle>) {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ratio_hop"
    ));
    let p = BacktestProvider::from_cache(
        &dir,
        &ids(),
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(BARS * HOUR),
        PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(10000)),
        },
    )
    .unwrap();
    (p.instruments().to_vec(), p.candles().to_vec())
}

fn scale(c: &Candle, f: Decimal) -> Candle {
    let s: CandleSpec = c.clone().into();
    Candle::new(CandleSpec {
        open: s.open * f,
        high: s.high * f,
        low: s.low * f,
        close: s.close * f,
        quote_volume: s.quote_volume * f,
        ..s
    })
    .unwrap()
}

/// Candles after `cut` (open time later than its close) rewritten.
fn rewrite_after(candles: &[Candle], cut: i64) -> Vec<Candle> {
    candles
        .iter()
        .map(|c| {
            if c.open_time.as_millis() < (cut + 1) * HOUR {
                c.clone()
            } else if c.instrument.symbol().as_str() == "BTCUSDT" {
                scale(c, dec!(0.5))
            } else {
                scale(c, dec!(2))
            }
        })
        .collect()
}

async fn run(instruments: &[bot_core::Instrument], candles: Vec<Candle>) -> RunReport {
    let provider = BacktestProvider::new(
        instruments.to_vec(),
        candles,
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(10000)),
        },
    );
    let params = Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10"
           freefall_pct = "0.5"
           freefall_candles = 2
           order_type = "market""#,
    )
    .unwrap();
    let mut s = strategy::build("ratio_hop", &params).unwrap();
    let config = EngineConfig {
        whitelist: ids(),
        history_len: 10,
    };
    engine::run(&provider, s.as_mut(), params, &config)
        .await
        .unwrap()
}

fn up_to(fills: &[Fill], cut: i64) -> Vec<Fill> {
    let horizon = Timestamp::from_millis(cut * HOUR + HOUR - 1);
    fills.iter().filter(|f| f.ts <= horizon).cloned().collect()
}

#[tokio::test]
async fn changing_the_future_never_changes_the_past() {
    let (instruments, candles) = fixture();
    let base = run(&instruments, candles.clone()).await;
    let mut future_mattered = false;
    for cut in 0..BARS {
        let alt = run(&instruments, rewrite_after(&candles, cut)).await;
        assert_eq!(
            up_to(&base.fills, cut),
            up_to(&alt.fills, cut),
            "fills up to t{cut} changed when only candles after t{cut} did"
        );
        let horizon = Timestamp::from_millis(cut * HOUR + HOUR - 1);
        let early = |r: &RunReport| -> Vec<_> {
            r.rejections
                .iter()
                .filter(|x| x.at.is_none_or(|t| t <= horizon))
                .cloned()
                .collect()
        };
        assert_eq!(early(&base), early(&alt), "rejections up to t{cut} changed");
        future_mattered |= base.fills != alt.fills;
    }
    // Guard against a vacuous pass: the rewrite must change *later* fills.
    assert!(
        future_mattered,
        "rewriting the future never changed any fill"
    );
}

/// The same property on a single-coin BTC fixture of `bars` candles: halving every candle
/// after t must leave every fill and rejection up to t's close unchanged.
async fn single_coin_property(fixture: &str, bars: i64, strategy: &str, params: &str) {
    let btc: Vec<InstrumentId> = vec!["binance:BTCUSDT".parse().unwrap()];
    let dir = DataDir::new(format!(
        "{}/tests/fixtures/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    ));
    let cached = BacktestProvider::from_cache(
        &dir,
        &btc,
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
    let (instruments, candles) = (cached.instruments().to_vec(), cached.candles().to_vec());
    let run = |candles: Vec<Candle>| {
        let instruments = instruments.clone();
        let btc = btc.clone();
        async move {
            let provider = BacktestProvider::new(
                instruments,
                candles,
                PaperConfig {
                    fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
                    slippage: BpsSlippage::new(dec!(10)).unwrap(),
                    initial_cash: Notional::new(dec!(10000)),
                },
            );
            let params = Params::from_toml(params).unwrap();
            let mut s = strategy::build(strategy, &params).unwrap();
            let config = EngineConfig {
                whitelist: btc,
                history_len: 10,
            };
            engine::run(&provider, s.as_mut(), params, &config)
                .await
                .unwrap()
        }
    };
    let base = run(candles.clone()).await;
    let mut future_mattered = false;
    for cut in 0..bars {
        let alt = run(rewrite_after(&candles, cut)).await;
        assert_eq!(
            up_to(&base.fills, cut),
            up_to(&alt.fills, cut),
            "{strategy} fills up to t{cut} changed when only candles after t{cut} did"
        );
        let horizon = Timestamp::from_millis(cut * HOUR + HOUR - 1);
        let early = |r: &RunReport| -> Vec<_> {
            r.rejections
                .iter()
                .filter(|x| x.at.is_none_or(|t| t <= horizon))
                .cloned()
                .collect()
        };
        assert_eq!(early(&base), early(&alt), "rejections up to t{cut} changed");
        future_mattered |= base.fills != alt.fills;
    }
    assert!(
        future_mattered,
        "rewriting the future never changed any {strategy} fill"
    );
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

/// The same property for momentum_rotation on its two-coin fixture (8
/// bars): BTC halved and ETH doubled after t must leave every fill and
/// rejection up to t's close unchanged.
#[tokio::test]
async fn momentum_rotation_never_lets_the_future_change_the_past() {
    const BARS: i64 = 8;
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/momentum_rotation"
    ));
    let cached = BacktestProvider::from_cache(
        &dir,
        &ids(),
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(BARS * HOUR),
        PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(10000)),
        },
    )
    .unwrap();
    let (instruments, candles) = (cached.instruments().to_vec(), cached.candles().to_vec());
    let run = |candles: Vec<Candle>| {
        let instruments = instruments.clone();
        async move {
            let provider = BacktestProvider::new(
                instruments,
                candles,
                PaperConfig {
                    fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
                    slippage: BpsSlippage::new(dec!(10)).unwrap(),
                    initial_cash: Notional::new(dec!(10000)),
                },
            );
            let params = Params::from_toml(
                r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
                   lookback = 2
                   quote_amount = "990.99"
                   fee = "0.001"
                   slippage_bps = "10""#,
            )
            .unwrap();
            let mut s = strategy::build("momentum_rotation", &params).unwrap();
            let config = EngineConfig {
                whitelist: ids(),
                history_len: 10,
            };
            engine::run(&provider, s.as_mut(), params, &config)
                .await
                .unwrap()
        }
    };
    let base = run(candles.clone()).await;
    let mut future_mattered = false;
    for cut in 0..BARS {
        let alt = run(rewrite_after(&candles, cut)).await;
        assert_eq!(
            up_to(&base.fills, cut),
            up_to(&alt.fills, cut),
            "momentum_rotation fills up to t{cut} changed when only candles after t{cut} did"
        );
        let horizon = Timestamp::from_millis(cut * HOUR + HOUR - 1);
        let early = |r: &RunReport| -> Vec<_> {
            r.rejections
                .iter()
                .filter(|x| x.at.is_none_or(|t| t <= horizon))
                .cloned()
                .collect()
        };
        assert_eq!(early(&base), early(&alt), "rejections up to t{cut} changed");
        future_mattered |= base.fills != alt.fills;
    }
    assert!(
        future_mattered,
        "rewriting the future never changed any momentum_rotation fill"
    );
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
