//! Restart property: stopping a run and starting it again from its saved
//! state must not change what it does.
//!
//! For each built-in strategy, on its regression fixture: run all bars
//! uninterrupted. Then, for every cut point k where no order is working,
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
//! restart after the reconcile rebuilds the same book.

mod common;

use bot_core::{Candle, Fill, InstrumentId, MarketEvent};
use common::{LastState, Replay, Shared, paper};
use engine::EngineConfig;
use exchange::Broker;
use exchange::paper::{BpsSlippage, RateFee};
use rust_decimal_macros::dec;
use strategy::Params;

/// A fill as it matters for P&L: everything but the order id.
fn essence(f: &Fill) -> String {
    format!(
        "{} {:?} {} @ {} fee {:?} {:?} t{}",
        f.instrument,
        f.side,
        f.qty,
        f.price,
        f.fee,
        f.liquidity,
        f.ts.as_millis()
    )
}

struct Case {
    strategy: &'static str,
    fixture: &'static str,
    coins: &'static [&'static str],
    bars: i64,
    params: &'static str,
}

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
    let instruments = cached.instruments().to_vec();
    let mut candles: Vec<Candle> = cached.candles().to_vec();
    candles.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));
    let config = EngineConfig {
        whitelist: ids.clone(),
        history_len: 20,
    };
    let params = Params::from_toml(c.params).unwrap();
    let per_bar = ids.len();

    // Uninterrupted.
    let base = {
        let provider = Replay {
            instruments: instruments.clone(),
            events: candles.iter().cloned().map(MarketEvent::Candle).collect(),
            broker: Shared(paper(&instruments), ""),
        };
        let mut s = strategy::build(c.strategy, &params).unwrap();
        engine::run(&provider, s.as_mut(), params.clone(), &config)
            .await
            .unwrap()
    };
    let expected: Vec<String> = base.fills.iter().map(essence).collect();

    let mut compared = 0;
    let mut compared_while_holding = 0;
    for k in 1..c.bars as usize {
        let split = k * per_bar;
        let broker = paper(&instruments);
        let first = Replay {
            instruments: instruments.clone(),
            events: candles[..split]
                .iter()
                .cloned()
                .map(MarketEvent::Candle)
                .collect(),
            broker: Shared(broker.clone(), ""),
        };
        let mut s1 = strategy::build(c.strategy, &params).unwrap();
        let mut last = LastState(None);
        let r1 = engine::run_observed(
            &first,
            s1.as_mut(),
            params.clone(),
            &config,
            &mut last,
            None,
        )
        .await
        .unwrap();
        if !broker.lock().await.open_orders().is_empty() {
            continue; // an order is working: a live restart would cancel it
        }
        let holding = {
            let b = broker.lock().await;
            ids.iter().any(|id| !b.position(id).is_flat())
        };
        assert!(last.0.is_some(), "{} saves state", c.strategy);

        let mut events: Vec<MarketEvent> = candles[..split]
            .iter()
            .cloned()
            .map(MarketEvent::Backfill)
            .collect();
        events.extend(candles[split..].iter().cloned().map(MarketEvent::Candle));
        let second = Replay {
            instruments: instruments.clone(),
            events,
            broker: Shared(broker.clone(), "r2-"),
        };
        let mut s2 = strategy::build(c.strategy, &params).unwrap();
        let mut ignore = LastState(None);
        let r2 = engine::run_observed(
            &second,
            s2.as_mut(),
            params.clone(),
            &config,
            &mut ignore,
            last.0,
        )
        .await
        .unwrap();

        let got: Vec<String> = r1.fills.iter().chain(&r2.fills).map(essence).collect();
        assert_eq!(
            got, expected,
            "{}: restart at bar {k} changed the fills",
            c.strategy
        );
        compared += 1;
        compared_while_holding += usize::from(holding);
    }
    assert!(compared > 0, "{}: no clean restart point", c.strategy);
    assert!(
        compared_while_holding > 0,
        "{}: never restarted while holding a position",
        c.strategy
    );
}

const BTC: &[&str] = &["binance:BTCUSDT"];
const BTC_ETH: &[&str] = &["binance:BTCUSDT", "binance:ETHUSDT"];

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
