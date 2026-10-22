//! momentum_rotation over a fixed 8-candle BTC/ETH fixture, through the
//! cache, BacktestProvider, the engine and the report.
//!
//! Fixture (`tests/fixtures/momentum_rotation`), flat 1h candles:
//!   BTC 100, 100, 110, 110, 115, 116, 114, 114 (tick 0.01)
//!   ETH  10,  10,  10, 10.5, 12.5, 12,  11,  11 (tick 0.001)
//! lookback 2, min_momentum 0, quote_amount 990.99. Fee 0.1% maker and
//! taker in the received asset; slippage 10 bps on taker fills, rounded to
//! the tick against the trader.

use backtest::{BacktestProvider, DataDir, Report, run_backtest};
use bot_core::{Fee, InstrumentId, Interval, Notional, Price, Qty, Side, Timestamp};
use engine::{EngineConfig, RunReport};
use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

const HOUR: i64 = 3_600_000;

fn ids() -> Vec<InstrumentId> {
    vec![
        "binance:BTCUSDT".parse().unwrap(),
        "binance:ETHUSDT".parse().unwrap(),
    ]
}

fn provider() -> BacktestProvider {
    let dir = DataDir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/momentum_rotation"
    ));
    BacktestProvider::from_cache(
        &dir,
        &ids(),
        Interval::H1,
        Timestamp::from_millis(0),
        Timestamp::from_millis(8 * HOUR),
        RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
        BpsSlippage::new(dec!(10)).unwrap(),
    )
    .unwrap()
}

fn params() -> Params {
    Params::from_toml(
        r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
           lookback = 2
           quote_amount = "990.99"
           fee = "0.001"
           slippage_bps = "10""#,
    )
    .unwrap()
}

fn config() -> EngineConfig {
    EngineConfig {
        whitelist: ids(),
        history_len: 10,
    }
}

async fn engine_run() -> RunReport {
    let p = params();
    let mut s = strategy::build("momentum_rotation", &p).unwrap();
    engine::run(&provider(), s.as_mut(), p, &config())
        .await
        .unwrap()
}

async fn report() -> Report {
    let p = params();
    let mut s = strategy::build("momentum_rotation", &p).unwrap();
    run_backtest(
        &provider(),
        s.as_mut(),
        p,
        &config(),
        Notional::new(dec!(990.99)),
        Interval::H1,
    )
    .await
    .unwrap()
}

/// Worked by hand (Python decimal, 28 digits); confirmed by owner, 2026-10-23.
///   t2: BTC (110-100)/100 = 0.10, ETH (10-10)/10 = 0 -> BTC: buy.
///   t3 buy BTC at 110 * 1.001 = 110.11: 990.99 / 110.11 = 9 BTC exactly, fee 0.009 BTC, hold 8.991, cash 0.
///   t3: BTC 0.10, ETH (10.5-10)/10 = 0.05 -> BTC held: nothing.
///   t4: BTC (115-110)/110 = 0.0454..., ETH (12.5-10)/10 = 0.25 -> ETH:
///      sell 8.991 BTC, then buy ETH.
///   t5 sell BTC at 116 * 0.999 = 115.884 -> 115.88: 1041.87708, fee
///      1.04187708, proceeds 1040.83520292, realized 49.84520292.
///   t5 buy ETH at 12 * 1.001 = 12.012: 990.99 / 12.012 = 82.5 ETH, fee
///      0.0825 ETH, hold 82.4175 at cost 990.99, cash 49.84520292.
///   t5: ETH (12-10.5)/10.5 = 0.1428... still leads, held: nothing.
///   t6: BTC (114-115)/115 < 0, ETH (11-12.5)/12.5 = -0.12: no coin beats
///      0 -> sell 82.4175 ETH.
///   t7 sell ETH at 11 * 0.999 = 10.989: 905.6859075, fee 0.9056859075,
///      proceeds 904.7802215925, realized -86.2097784075; flat.
///   t7: both still negative, flat: nothing.
/// Realized -36.3645754875; final cash 954.6254245125 (flat). Equity at
/// closes: 990.99 x3, 989.01, 1033.965, 1038.85520292, 956.43770292,
/// 954.6254245125. Max drawdown from the 1038.85520292 peak =
/// 0.081079421050 (12 dp). Sharpe -11.926392.
#[tokio::test]
async fn enters_rotates_and_goes_to_cash() {
    let r = engine_run().await;
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.fills.len(), 4);
    let btc = ids()[0].clone();
    let eth = ids()[1].clone();
    let at = |n: i64| Timestamp::from_millis(n * HOUR);

    let f = &r.fills;
    assert_eq!(
        (&f[0].instrument, f[0].side, f[0].ts, f[0].price, f[0].qty),
        (
            &btc,
            Side::Buy,
            at(3),
            Price::new(dec!(110.11)),
            Qty::new(dec!(9))
        )
    );
    assert_eq!(f[0].fee, Fee::Base(Qty::new(dec!(0.009))));
    assert_eq!(
        (&f[1].instrument, f[1].side, f[1].ts, f[1].price, f[1].qty),
        (
            &btc,
            Side::Sell,
            at(5),
            Price::new(dec!(115.88)),
            Qty::new(dec!(8.991))
        )
    );
    assert_eq!(f[1].fee, Fee::Quote(Notional::new(dec!(1.04187708))));
    assert_eq!(
        (&f[2].instrument, f[2].side, f[2].ts, f[2].price, f[2].qty),
        (
            &eth,
            Side::Buy,
            at(5),
            Price::new(dec!(12.012)),
            Qty::new(dec!(82.5))
        )
    );
    assert_eq!(f[2].fee, Fee::Base(Qty::new(dec!(0.0825))));
    assert_eq!(
        (&f[3].instrument, f[3].side, f[3].ts, f[3].price, f[3].qty),
        (
            &eth,
            Side::Sell,
            at(7),
            Price::new(dec!(10.989)),
            Qty::new(dec!(82.4175))
        )
    );
    assert_eq!(f[3].fee, Fee::Quote(Notional::new(dec!(0.9056859075))));

    let (pb, pe) = (&r.positions[0], &r.positions[1]);
    assert!(pb.is_flat() && pe.is_flat());
    assert_eq!(pb.realized_pnl(), Notional::new(dec!(49.84520292)));
    assert_eq!(pe.realized_pnl(), Notional::new(dec!(-86.2097784075)));
}

#[tokio::test]
async fn report_matches_the_hand_worked_run() {
    let r = report().await;
    assert_eq!(r.fills, 4);
    assert_eq!(r.closed_trades, 2);
    assert_eq!(r.win_rate, Some(dec!(0.5)));
    assert_eq!(r.rejected_signals, 0);
    assert_eq!(r.final_equity, Notional::new(dec!(954.6254245125)));
    assert_eq!(r.realized_pnl, Notional::new(dec!(-36.3645754875)));
    // The BTC buy spends the whole budget; the ETH buy is paid from the
    // larger BTC proceeds, so cash never goes below 0.
    assert_eq!(r.min_cash, Notional::ZERO);
    assert_eq!(r.max_drawdown.round_dp(12), dec!(0.081079421050));
    assert_eq!(r.sharpe.unwrap().round_dp(6), dec!(-11.926392));
}
