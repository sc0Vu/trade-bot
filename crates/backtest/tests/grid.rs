//! grid through the engine, the paper broker and the report, on four bars
//! worked by hand.
//!
//! Grid 90..110 in 4 equal gaps (levels 90, 95, 100, 105, 110), 100 USDT
//! per buy, 1000 USDT cash. Fee 0.1% maker and taker in the received
//! asset; slippage 10 bps on taker fills, to the tick against the trader,
//! never past the limit. Instrument BTCUSDT filters: tick 0.01, lot
//! 0.00001.

mod common;

use backtest::{BacktestProvider, DataDir, PaperConfig, run_backtest};
use bot_core::{
    Candle, CandleSpec, InstrumentId, Interval, Liquidity, MarketEvent, Notional, Price, Qty, Side,
    Timestamp,
};
use common::{LastState, Replay, Shared, paper};
use engine::EngineConfig;
use exchange::Broker;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn btc() -> InstrumentId {
    "binance:BTCUSDT".parse().unwrap()
}

fn bar(i: i64, [o, h, l, c]: [&str; 4]) -> Candle {
    let p = |s: &str| s.parse::<Price>().unwrap();
    Candle::new(CandleSpec {
        instrument: btc(),
        interval: Interval::H1,
        open_time: Timestamp::from_millis(i * HOUR),
        close_time: Timestamp::from_millis(i * HOUR + HOUR - 1),
        open: p(o),
        high: p(h),
        low: p(l),
        close: p(c),
        volume: Qty::new(dec!(1)),
        quote_volume: Notional::new(p(c).get()),
    })
    .unwrap()
}

fn instrument() -> bot_core::Instrument {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ratio_hop"
    ));
    dir.read_instrument(&btc()).unwrap()
}

fn bars() -> Vec<Candle> {
    vec![
        bar(0, ["100", "100", "100", "100"]),
        bar(1, ["100", "101", "99", "100"]),
        bar(2, ["100", "106", "100", "105"]),
        bar(3, ["105", "105", "99.5", "100"]),
    ]
}

fn provider() -> BacktestProvider {
    BacktestProvider::new(
        vec![instrument()],
        bars(),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(1000)),
        },
    )
}

fn params() -> Params {
    Params::from_toml(
        r#"coin = "binance:BTCUSDT"
           lower = "90"
           upper = "110"
           grids = 4
           spacing = "arithmetic"
           quote_per_grid = "100"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: vec![btc()],
        history_len: 10,
    }
}

/// Worked by hand:
///   t0 close 100: buys at 90, 95, 100, 105 (100 USDT each).
///   t1 (o100 h101 l99): the 105 buy is marketable, fills at the open
///      slipped, 100 * 1.001 = 100.10 (taker), lot(100/105) = 0.95238 BTC:
///      cost 95.333238, net 0.95142762. The 100 buy rests and the low 99
///      trades through: 1 BTC at 100 (maker), net 0.999. Close 100: sells
///      of 0.999 at 105 and lot(0.95142762) = 0.95142 at 110.
///   t2 (h106): the 105 sell fills (maker): 104.895 - 0.104895 =
///      104.790105. Close 105: that gap buys at 100 again.
///   t3 (l99.5): the 100 buy fills (maker), 1 BTC, net 0.999.
/// Cash 1000 - 95.333238 - 100 + 104.790105 - 100 = 809.456867 (lowest
/// 804.666762, after t1). Held 0.95142762 + 0.999 + 0.999 - 0.999 =
/// 1.95042762 BTC at 100: equity 1004.499629.
#[tokio::test]
async fn buys_low_sells_one_level_up_and_buys_again() {
    let p = params();
    let mut s = strategy::build("grid", &p).unwrap();
    let r = engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap();
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    let got: Vec<_> = r
        .fills
        .iter()
        .map(|f| (f.ts, f.side, f.qty, f.price, f.liquidity))
        .collect();
    // A taker fill is stamped at its bar's open, a maker fill (traded
    // through during the bar) at its close; within a bar, in submission
    // order (the 100 buy went in before the 105 buy).
    let open = |i: i64| Timestamp::from_millis(i * HOUR);
    let close = |i: i64| Timestamp::from_millis((i + 1) * HOUR - 1);
    let (q, px) = (Qty::new, Price::new);
    assert_eq!(
        got,
        vec![
            (
                close(1),
                Side::Buy,
                q(dec!(1)),
                px(dec!(100)),
                Liquidity::Maker
            ),
            (
                open(1),
                Side::Buy,
                q(dec!(0.95238)),
                px(dec!(100.10)),
                Liquidity::Taker
            ),
            (
                close(2),
                Side::Sell,
                q(dec!(0.999)),
                px(dec!(105)),
                Liquidity::Maker
            ),
            (
                close(3),
                Side::Buy,
                q(dec!(1)),
                px(dec!(100)),
                Liquidity::Maker
            ),
        ]
    );
    assert_eq!(r.positions[0].qty(), q(dec!(1.95042762)));

    let p = params();
    let mut s = strategy::build("grid", &p).unwrap();
    let report = run_backtest(&provider(), s.as_mut(), p, &config(), Interval::H1)
        .await
        .unwrap();
    assert_eq!(report.final_equity, Notional::new(dec!(1004.499629)));
    assert_eq!(report.min_cash, Notional::new(dec!(804.666762)));
    assert_eq!(report.closed_trades, 1);
}

/// A grid always has orders working, so the idle-cut restart test
/// (`restart.rs`) never applies; this is its restart property instead. A
/// live restart's reconcile cancels every working order: the restored grid
/// must put back exactly the book it had. Cut after t2 (see above): buys at
/// 90, 95 and 100 (that gap bought again at t2) and the 0.95142 sell at
/// 110; held 0.95142762, of which 0.00000762 dust, as saved. Restored and
/// warmed up on t0..t2 as Backfill, at t3 it places the same four orders.
#[tokio::test]
async fn a_restart_rebuilds_the_same_book() {
    let book = |orders: Vec<bot_core::Order>| {
        let mut v: Vec<String> = orders
            .iter()
            .map(|o| format!("{:?} {:?} {:?}", o.side, o.size, o.kind))
            .collect();
        v.sort();
        v
    };
    let broker = paper(&[instrument()]);
    let first = Replay {
        instruments: vec![instrument()],
        events: bars()[..3]
            .iter()
            .cloned()
            .map(MarketEvent::Candle)
            .collect(),
        broker: Shared(broker.clone(), ""),
    };
    let p = params();
    let mut s1 = strategy::build("grid", &p).unwrap();
    let mut last = LastState(None);
    engine::run_observed(&first, s1.as_mut(), p.clone(), &config(), &mut last, None)
        .await
        .unwrap();
    let before = broker.lock().await.open_orders();
    assert_eq!(before.len(), 4);
    for o in &before {
        broker.lock().await.cancel(o.id.clone()).await.unwrap();
    }

    let mut events: Vec<MarketEvent> = bars()[..3]
        .iter()
        .cloned()
        .map(MarketEvent::Backfill)
        .collect();
    events.push(MarketEvent::Candle(bars()[3].clone()));
    let second = Replay {
        instruments: vec![instrument()],
        events,
        broker: Shared(broker.clone(), "r2-"),
    };
    let mut s2 = strategy::build("grid", &p).unwrap();
    let r2 = engine::run_observed(
        &second,
        s2.as_mut(),
        p,
        &config(),
        &mut LastState(None),
        last.0,
    )
    .await
    .unwrap();
    assert!(r2.fills.is_empty() && r2.rejections.is_empty());
    let after = broker.lock().await.open_orders();
    assert_eq!(book(after), book(before));
}
