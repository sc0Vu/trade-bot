//! Delistings through the whole backtest path: the reference file's
//! announcements, gap cuts with no notice, the engine's exit, and the
//! strategies carrying on with the coins left.
//!
//! BTC + ETH, 1h, flat candles; `rebalance` 50/50 of 10000, band 0.05,
//! no fees or slippage, so every number is plain arithmetic.

use backtest::{BacktestProvider, Delisting, PaperConfig};
use bot_core::{
    Asset, AssetClass, Candle, CandleSpec, Fill, Instrument, InstrumentSpec, Interval, Notional,
    Price, Qty, Side, Timestamp,
};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, RateFee};
use rust_decimal_macros::dec;
use std::time::Duration;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn inst(symbol: &str, base: &str) -> Instrument {
    Instrument::new(InstrumentSpec {
        id: format!("binance:{symbol}").parse().unwrap(),
        class: AssetClass::Spot,
        base: Asset::new(base).unwrap(),
        quote: Asset::new("USDT").unwrap(),
        tick_size: Price::new(dec!(0.01)),
        lot_size: Qty::new(dec!(0.00001)),
        min_qty: Qty::new(dec!(0.00001)),
        min_notional: Notional::new(dec!(5)),
        max_open_orders: None,
        max_open_stops: None,
        price_band: None,
    })
    .unwrap()
}

fn bar(i: &Instrument, n: i64, px: &str) -> Candle {
    let p: Price = px.parse().unwrap();
    Candle::new(CandleSpec {
        instrument: i.id.clone(),
        interval: Interval::H1,
        open_time: Timestamp::from_millis(n * HOUR),
        close_time: Timestamp::from_millis((n + 1) * HOUR - 1),
        open: p,
        high: p,
        low: p,
        close: p,
        volume: Qty::new(dec!(1)),
        quote_volume: Notional::new(p.get()),
    })
    .unwrap()
}

/// BTC at 100 for bars 0..10, jumping to 150 from bar 6; ETH at 10 for
/// `eth_bars`.
fn provider(eth_bars: std::ops::Range<i64>) -> BacktestProvider {
    let (btc, eth) = (inst("BTCUSDT", "BTC"), inst("ETHUSDT", "ETH"));
    let mut candles: Vec<Candle> = (0..10)
        .map(|n| bar(&btc, n, if n >= 6 { "150" } else { "100" }))
        .collect();
    candles.extend(eth_bars.map(|n| bar(&eth, n, "10")));
    BacktestProvider::new(
        vec![btc, eth],
        candles,
        PaperConfig {
            fees: RateFee::zero(),
            slippage: BpsSlippage::zero(),
            initial_cash: Notional::new(dec!(10000)),
        },
    )
}

async fn fills(p: &BacktestProvider) -> Vec<Fill> {
    let params = Params::from_toml(
        r#"weights = { "binance:BTCUSDT" = "0.5", "binance:ETHUSDT" = "0.5" }
           capital = "10000"
           band = "0.05"
           fee = "0"
           slippage_bps = "0""#,
    )
    .unwrap();
    let mut s = strategy::build("rebalance", &params).unwrap();
    let config = EngineConfig {
        whitelist: p.instruments().iter().map(|i| i.id.clone()).collect(),
        history_len: 10,
        ..Default::default()
    };
    let r = engine::run(p, s.as_mut(), params, &config).await.unwrap();
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    r.fills
}

fn summary(fills: &[Fill]) -> Vec<(i64, String, Side)> {
    fills
        .iter()
        .map(|f| {
            (
                f.ts.as_millis() / HOUR,
                f.instrument.symbol().to_string(),
                f.side,
            )
        })
        .collect()
}

/// ETH's data ends after bar 3. Uncut, rebalance waits for an ETH candle
/// that never comes: after bar 3 it never decides again, so BTC's jump at
/// bar 6 (weight 0.6 > 0.55) goes unanswered. Cut (gap limit 2h, end at
/// 10h), ETH delists right after its last bar with no notice; the 5000 of
/// ETH is stuck at 10, and the rebalance still answers the jump: sell BTC
/// back to half of 12500 (decided at bar 6, filled at bar 7's open).
#[tokio::test]
async fn a_coin_that_stops_doesnt_halt_the_strategy() {
    let uncut = fills(&provider(0..4)).await;
    assert_eq!(
        summary(&uncut),
        vec![
            (1, "BTCUSDT".into(), Side::Buy),
            (1, "ETHUSDT".into(), Side::Buy)
        ]
    );

    let cut = provider(0..4).cut_at_gaps(
        Duration::from_secs(2 * 3600),
        Timestamp::from_millis(10 * HOUR),
    );
    assert_eq!(cut.events().len(), 1);
    let got = fills(&cut).await;
    assert_eq!(
        summary(&got),
        vec![
            (1, "BTCUSDT".into(), Side::Buy),
            (1, "ETHUSDT".into(), Side::Buy),
            (7, "BTCUSDT".into(), Side::Sell),
        ]
    );
    // 50 BTC at 150 = 7500, ETH 5000 stuck: total 12500, BTC to 6250.
    assert_eq!(got[2].qty, Qty::new(dec!(8.33333)));
}

/// ETH's delisting is announced at 2h for 6h; ETH data exists through
/// bar 9 (as a reused symbol's would), but bars from 6h on are dropped.
/// The announcement is already inside the default 24h exit window, so the
/// engine sells the 500 ETH at once (decided at 2h, filled at bar 3's
/// open: bar 2 opened at 2h, not after it). The exit's own sell, still
/// working at the next events, is waited for, not cancelled and re-sent.
/// Nothing is traded on ETH after that.
#[tokio::test]
async fn an_announced_delisting_is_left_before_it_happens() {
    let p = provider(0..10).with_delistings(&[Delisting {
        instrument: "binance:ETHUSDT".parse().unwrap(),
        announced: Timestamp::from_millis(2 * HOUR),
        at: Timestamp::from_millis(6 * HOUR),
        source: "test".into(),
    }]);
    assert!(
        p.candles()
            .iter()
            .all(|c| c.instrument.symbol().as_str() != "ETHUSDT"
                || c.open_time.as_millis() < 6 * HOUR)
    );
    let got = fills(&p).await;
    let eth: Vec<_> = summary(&got)
        .into_iter()
        .filter(|(_, s, _)| s == "ETHUSDT")
        .collect();
    assert_eq!(
        eth,
        vec![
            (1, "ETHUSDT".into(), Side::Buy),
            (3, "ETHUSDT".into(), Side::Sell)
        ]
    );
    assert!(
        got.iter()
            .all(|f| f.ts.as_millis() < 6 * HOUR || f.instrument.symbol().as_str() == "BTCUSDT")
    );
}
