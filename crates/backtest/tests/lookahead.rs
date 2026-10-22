//! No-lookahead property (invariant 2), end to end: changing the future
//! must not change the past.
//!
//! For every cut point t, all candles after t are replaced (BTC halved,
//! ETH doubled) and ratio_hop runs on both feeds. Every fill stamped at or
//! before t's close must be identical: those fills happen on candles up to
//! t, from decisions made on candles before them, so nothing later may
//! influence them. A strategy, engine or broker that peeked ahead would
//! break this.

use backtest::{BacktestProvider, DataDir};
use bot_core::{Candle, CandleSpec, Decimal, Fill, InstrumentId, Interval, Timestamp};
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
        RateFee::zero(),
        BpsSlippage::zero(),
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
        RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
        BpsSlippage::new(dec!(10)).unwrap(),
    );
    let params = Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10"
           freefall_pct = "0.5"
           freefall_candles = 2"#,
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
