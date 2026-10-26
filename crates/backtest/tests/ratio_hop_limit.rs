//! ratio_hop in limit mode: every leg a GTC limit at the last close.
//!
//! Fixture (`tests/fixtures/ratio_hop_limit`): same closes as the market
//! fixture (BTC 100; ETH 10, 10, 8, 8, 10, 10, 10) but every bar has
//! wicks (BTC 99-101, ETH close +-0.1; t2 low 7.9, t4 opens 8, low 7.9,
//! high 10.1), so a limit at the previous close is traded through and
//! fills, maker, at the limit. Fee 0.1%; slippage 10 bps (taker only, so
//! unused here: no limit is marketable at an open equal to it).

use backtest::{BacktestProvider, DataDir, PaperConfig, Report, run_backtest};
use bot_core::{InstrumentId, Interval, Notional, Timestamp};
use engine::EngineConfig;
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

fn ids() -> Vec<InstrumentId> {
    vec![
        "binance:BTCUSDT".parse().unwrap(),
        "binance:ETHUSDT".parse().unwrap(),
    ]
}

async fn run(fixture: &str, limit_timeout: u32) -> Report {
    let dir = DataDir::new(format!(
        "{}/tests/fixtures/{fixture}",
        env!("CARGO_MANIFEST_DIR")
    ));
    let provider = BacktestProvider::from_cache(
        &dir,
        &ids(),
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(7 * 3_600_000),
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(1000)),
        },
    )
    .unwrap();
    let params = Params::from_toml(&format!(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10"
           freefall_pct = "0.5"
           freefall_candles = 2
           order_type = "limit"
           limit_timeout = {limit_timeout}"#
    ))
    .unwrap();
    let mut s = strategy::build("ratio_hop", &params).unwrap();
    let config = EngineConfig {
        whitelist: ids(),
        history_len: 10,
    };
    run_backtest(&provider, s.as_mut(), params, &config, Interval::H1)
        .await
        .unwrap()
}

/// Worked by hand, confirmed by owner 2026-10-22. Each leg is a limit at the last close; the next bar
/// trades through it, so it fills at the limit as maker, stamped at that
/// bar's close. Estimates use closes with no slippage (limits can't slip):
///   t0 limit buy BTC @100, budget 1000 -> 10 BTC; t1 low 99 < 100 fills:
///      spend 1000, fee 0.01 BTC, hold 9.99, cash 0
///   t1 baselines BTC 9.99, ETH lot(9.99*100*0.999/10)*0.999 = 99.7002999
///   t2 ETH 8: est 124.62536988 > 99.7002999 -> limit sell 9.99 BTC @100
///   t3 high 101 > 100 fills: 999 - 0.999 = 998.001; limit buy ETH @8 (ETH's
///      last close), budget 998.001 -> 124.75012 ETH
///   t4 low 7.9 < 8 fills: spend 998.00096, hold 124.62536988; ETH 10:
///      est BTC lot(124.62536*10*0.999/100)*0.999 = 12.43758996 > 9.99 ->
///      limit sell 124.62536 ETH @10
///   t5 high 10.1 > 10 fills: 1246.2536 - 1.2462536 = 1245.0073464; limit
///      buy BTC @100 -> 12.45007 BTC
///   t6 low 99 fills: spend 1245.007, hold 12.43761993; est ETH
///      124.12746828 <= baseline 124.62536 -> hold
/// Same units as the zero-slippage market run, so equity at closes is
/// 1000, 999, 999, 998.001, 1246.2537388, 1245.0074852, 1243.7624782:
/// max drawdown 0.001999, Sharpe 37.474704 (Python decimal check).
/// Cash after fills: 0, 998.001, 0.00004, 1245.0073864, 0.0003864.
#[tokio::test]
async fn limit_legs_fill_as_maker_when_traded_through() {
    let r = run("ratio_hop_limit", 3).await;
    assert_eq!(r.fills, 5);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.win_rate, Some(dec!(0.5)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(1243.7624782)));
    assert_eq!(r.realized_pnl.get().round_dp(10), dec!(245.0074655191));
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown, dec!(0.001999));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(37.474704));
}

/// On the flat market fixture (every bar o=h=l=c), a limit at the close is
/// only ever touched, never traded through, so it never fills. With
/// limit_timeout 2 the strategy cancels and re-prices it every two
/// evaluations; every cancel names a live order, so nothing is rejected.
#[tokio::test]
async fn untraded_limit_is_cancelled_and_repriced_never_filled() {
    let r = run("ratio_hop", 2).await;
    assert_eq!(r.fills, 0);
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(1000)));
    assert_eq!(r.min_cash, Notional::new(dec!(1000)));
}

/// BTC drifts up with no wicks (t0 100, t1 101, t2 102, t3 102), so the
/// first limit buy at 100 is never traded through. At t2 (limit_timeout 2)
/// it is cancelled; at t3, the cancel being final (no fill can still
/// arrive), it is placed again at that close, 102. t4 (low 101 < 102)
/// fills it there, maker. Only a re-priced order can fill at 102: qty
/// lot(1000/102) = 9.80392, spend 999.99984 <= 1000.
#[tokio::test]
async fn stale_limit_is_repriced_to_the_new_close() {
    use bot_core::{Candle, CandleSpec, Liquidity, Price, Qty};

    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ratio_hop"
    ));
    let instruments: Vec<_> = ids()
        .iter()
        .map(|id| dir.read_instrument(id).unwrap())
        .collect();
    let bar = |id: &InstrumentId, i: i64, [o, h, l, c]: [&str; 4]| {
        let p = |s: &str| s.parse::<Price>().unwrap();
        Candle::new(CandleSpec {
            instrument: id.clone(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(i * 3_600_000),
            close_time: Timestamp::from_millis(i * 3_600_000 + 3_599_999),
            open: p(o),
            high: p(h),
            low: p(l),
            close: p(c),
            volume: Qty::new(dec!(1)),
            quote_volume: Notional::new(p(c).get()),
        })
        .unwrap()
    };
    let (btc, eth) = (&ids()[0], &ids()[1]);
    let mut candles = vec![
        bar(btc, 0, ["100", "100", "100", "100"]),
        bar(btc, 1, ["101", "101", "101", "101"]),
        bar(btc, 2, ["102", "102", "102", "102"]),
        bar(btc, 3, ["102", "102", "102", "102"]),
        bar(btc, 4, ["102", "103", "101", "102"]),
    ];
    candles.extend((0..5).map(|i| bar(eth, i, ["10", "10", "10", "10"])));
    let provider = BacktestProvider::new(
        instruments,
        candles,
        PaperConfig {
            fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
            slippage: BpsSlippage::new(dec!(10)).unwrap(),
            initial_cash: Notional::new(dec!(1000)),
        },
    );
    let params = Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           quote_amount = "1000"
           fee = "0.001"
           slippage_bps = "10"
           freefall_pct = "0.5"
           freefall_candles = 2
           order_type = "limit"
           limit_timeout = 2"#,
    )
    .unwrap();
    let mut s = strategy::build("ratio_hop", &params).unwrap();
    let config = EngineConfig {
        whitelist: ids(),
        history_len: 10,
    };
    let r = engine::run(&provider, s.as_mut(), params, &config)
        .await
        .unwrap();

    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 1);
    let f = &r.fills[0];
    assert_eq!(f.price, Price::new(dec!(102)));
    assert_eq!(f.liquidity, Liquidity::Maker);
    assert_eq!(f.qty, Qty::new(dec!(9.80392)));
}
