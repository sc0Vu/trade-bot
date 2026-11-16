//! Acceptance test for script strategies: `strategies/sma_trend.rhai`
//! must give exactly the fills of the built-in `sma_trend` it ports, run
//! through the same engine and paper broker (fees, slippage, tick
//! rounding), on a long deterministic random walk with many round trips.

#![cfg(feature = "rhai")]

mod common;

use bot_core::{Candle, CandleSpec, Fill, Interval, MarketEvent, Notional, Price, Qty, Timestamp};
use common::{Replay, Shared, paper};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, RateFee};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;
const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../strategies/sma_trend.rhai"
);

/// `n` hourly BTC bars: a seeded walk of up to ±1.5% a bar, on the 0.01
/// tick, wicks up to 0.5% beyond open/close. No randomness at run time.
fn walk(n: i64) -> Vec<Candle> {
    let mut seed: u64 = 0x5eed;
    let mut next = move |m: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % m
    };
    let mut close = dec!(100);
    (0..n)
        .map(|i| {
            let open = close;
            let step = Decimal::from(next(301) as i64 - 150) / dec!(10000);
            close = (open * (Decimal::ONE + step)).round_dp(2);
            let top = open.max(close);
            let bottom = open.min(close);
            let high = (top * (Decimal::ONE + Decimal::from(next(51)) / dec!(10000))).round_dp(2);
            let low = (bottom * (Decimal::ONE - Decimal::from(next(51)) / dec!(10000))).round_dp(2);
            Candle::new(CandleSpec {
                instrument: "binance:BTCUSDT".parse().unwrap(),
                interval: Interval::H1,
                open_time: Timestamp::from_millis(i * HOUR),
                close_time: Timestamp::from_millis(i * HOUR + HOUR - 1),
                open: Price::new(open),
                high: Price::new(high),
                low: Price::new(low),
                close: Price::new(close),
                volume: Qty::new(dec!(1)),
                quote_volume: Notional::new(close),
            })
            .unwrap()
        })
        .collect()
}

async fn fills(strategy: &str, params: &Params, candles: &[Candle]) -> Vec<Fill> {
    // Instrument reference data (tick 0.01, lot 0.00001) from a fixture.
    let dir = backtest::DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sma_trend"
    ));
    let ids = vec!["binance:BTCUSDT".parse().unwrap()];
    let cached = backtest::BacktestProvider::from_cache(
        &dir,
        &ids,
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(HOUR),
        backtest::PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(1)),
        },
    )
    .unwrap();
    let instruments = cached.instruments().to_vec();
    let provider = Replay {
        instruments: instruments.clone(),
        events: candles.iter().cloned().map(MarketEvent::Candle).collect(),
        broker: Shared(paper(&instruments), ""),
    };
    let config = EngineConfig {
        whitelist: ids,
        history_len: 60,
    };
    let mut s = strategy::build(strategy, params).unwrap();
    engine::run(&provider, s.as_mut(), params.clone(), &config)
        .await
        .unwrap()
        .fills
}

#[tokio::test]
async fn sma_trend_script_fills_like_the_builtin() {
    let candles = walk(2000);
    for (period, band) in [(3, "0"), (20, "0.002"), (50, "0.01")] {
        let common = format!(
            "coin = \"binance:BTCUSDT\"\nsma_period = {period}\nquote_amount = \"1000\"\n\
             band_pct = \"{band}\"\nfee = \"0.001\"\nslippage_bps = \"10\""
        );
        let native = fills("sma_trend", &Params::from_toml(&common).unwrap(), &candles).await;
        let scripted = fills(
            "rhai",
            &Params::from_toml(&format!("script = {SCRIPT:?}\n{common}")).unwrap(),
            &candles,
        )
        .await;
        assert!(
            native.len() >= 10,
            "period {period}: only {} fills, too few to compare",
            native.len()
        );
        assert_eq!(scripted, native, "period {period}, band {band}");
    }
}
