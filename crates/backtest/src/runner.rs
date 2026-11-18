use crate::{BacktestProvider, Report};
use bot_core::Interval;
use engine::{EngineConfig, EngineError};
use strategy::{Params, Strategy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BacktestError {
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs `strategy` over the provider's candles through the shared engine
/// loop and reports the result. The provider's initial cash is both the
/// broker's balance and the report's starting equity; `interval`
/// annualizes Sharpe.
pub async fn run_backtest(
    provider: &BacktestProvider,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
    interval: Interval,
) -> Result<Report, BacktestError> {
    let run = engine::run(provider, strategy, params, config).await?;
    Ok(Report::compute_full(
        &run,
        provider.candles(),
        provider.events(),
        provider.instruments(),
        provider.initial_cash(),
        interval,
        provider.earn(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaperConfig;
    use crate::test_support::{btcusdt, candle};
    use bot_core::Notional;
    use exchange::paper::{BpsSlippage, FeeAsset, RateFee};
    use rust_decimal_macros::dec;

    /// Owner-confirmed engine scenario (see engine tests), now through
    /// BacktestProvider and the report: 1000 USDT budget, fee 0.1% in base,
    /// no slippage -> spend 999.99999, hold 9.89108901 BTC.
    /// Equity at c2 close 103: cash 0.00001 + 9.89108901 * 103
    /// (= 1018.78216803) = 1018.78217803.
    #[tokio::test]
    async fn buy_and_hold_through_backtest_provider() {
        let id = btcusdt().id.clone();
        let provider = BacktestProvider::new(
            vec![btcusdt()],
            vec![
                candle(&id, 0, ["100", "101", "99", "100"]),
                candle(&id, 1, ["101", "103", "100", "102"]),
                candle(&id, 2, ["102", "104", "101", "103"]),
            ],
            PaperConfig {
                fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap(),
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        );
        let params = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\nquote_amount = \"1000\"\nfee = \"0.001\"\nslippage_bps = \"0\"",
        )
        .unwrap();
        let mut s = strategy::build("buy_and_hold", &params).unwrap();
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            ..Default::default()
        };
        let r = run_backtest(&provider, s.as_mut(), params, &config, Interval::H1)
            .await
            .unwrap();
        assert_eq!(r.fills, 1);
        assert_eq!(r.closed_trades, 0);
        assert_eq!(r.realized_pnl, Notional::ZERO);
        assert_eq!(r.final_equity, Notional::new(dec!(1018.78217803)));
        // 1000 budget, 999.99999 spent: never below zero.
        assert_eq!(r.min_cash, Notional::new(dec!(0.00001)));
    }

    /// Records the strategy's view of realized P&L at each funding event,
    /// after the engine settled it.
    struct Watch {
        inner: Box<dyn Strategy>,
        seen: Vec<Notional>,
    }

    impl Strategy for Watch {
        fn name(&self) -> &str {
            "watch"
        }
        fn on_event(
            &mut self,
            ctx: &mut strategy::Context,
            e: &bot_core::MarketEvent,
        ) -> Vec<bot_core::Signal> {
            if let bot_core::MarketEvent::Funding { instrument, .. } = e {
                self.seen.push(ctx.position(instrument).realized_pnl());
            }
            self.inner.on_event(ctx, e)
        }
        fn on_fill(&mut self, ctx: &mut strategy::Context, f: &bot_core::Fill) {
            self.inner.on_fill(ctx, f)
        }
    }

    /// buy_and_hold on a perpetual, isolated 2x, wallet 1,000, taker 0.1%
    /// in quote: the 1,000 budget buys 10 @ 100 at t1's open (fee 1,
    /// margin 500). Funding 0.1% at mark 110 at t2's open: the long pays
    /// 1.1, and the strategy sees realized −2.1 at that event. Equity at
    /// t2's close 120: 1,000 − 1 − 1.1 + 10 × (120 − 100) = 1,197.9.
    #[tokio::test]
    async fn a_perpetual_backtest_runs_through_the_engine_with_funding() {
        use bot_core::margin::{Bracket, Brackets, MarginMode};
        use bot_core::{AssetClass, Instrument, InstrumentSpec, MarketEvent, Price, Timestamp};
        use exchange::paper::PerpTerms;
        use std::collections::HashMap;
        let perp = Instrument::new(InstrumentSpec {
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let id = perp.id.clone();
        let brackets = Brackets::new(vec![Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(dec!(1000000)),
            max_leverage: 20,
            rate: dec!(0.004),
            cum: dec!(0),
        }])
        .unwrap();
        let provider = BacktestProvider::new(
            vec![perp],
            vec![
                candle(&id, 0, ["100", "100", "100", "100"]),
                candle(&id, 1, ["100", "110", "100", "110"]),
                candle(&id, 2, ["110", "120", "110", "120"]),
            ],
            PaperConfig {
                fees: RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Quote).unwrap(),
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        )
        .with_perp(crate::PerpSetup {
            terms: HashMap::from([(
                id.clone(),
                PerpTerms {
                    leverage: 2,
                    mode: MarginMode::Isolated,
                    brackets,
                },
            )]),
            clearance: dec!(0.0125),
        })
        .with_events(vec![MarketEvent::Funding {
            instrument: id.clone(),
            rate: dec!(0.001),
            mark: Price::new(dec!(110)),
            ts: Timestamp::from_millis(2 * crate::test_support::HOUR),
        }]);
        let params = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\nquote_amount = \"1000\"\nfee = \"0.001\"\nslippage_bps = \"0\"",
        )
        .unwrap();
        let mut s = Watch {
            inner: strategy::build("buy_and_hold", &params).unwrap(),
            seen: Vec::new(),
        };
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            ..Default::default()
        };
        let r = run_backtest(&provider, &mut s, params, &config, Interval::H1)
            .await
            .unwrap();
        assert_eq!(r.fills, 1);
        assert_eq!(s.seen, vec![Notional::new(dec!(-2.1))]);
        assert_eq!(r.funding, Notional::new(dec!(-1.1)));
        assert_eq!(r.final_equity, Notional::new(dec!(1197.9)));
    }

    /// Sells 1 (base) at t0's close and again at t2's close.
    struct Shorter;

    impl Strategy for Shorter {
        fn name(&self) -> &str {
            "shorter"
        }
        fn on_candle(
            &mut self,
            _ctx: &mut strategy::Context,
            c: &bot_core::Candle,
        ) -> Vec<bot_core::Signal> {
            let i = c.open_time.as_millis() / crate::test_support::HOUR;
            if i != 0 && i != 2 {
                return vec![];
            }
            vec![bot_core::Signal::Submit {
                instrument: c.instrument.clone(),
                side: bot_core::Side::Sell,
                size: bot_core::Size::Base(bot_core::Qty::new(dec!(1))),
                kind: bot_core::OrderKind::Market,
            }]
        }
    }

    /// A perpetual short under a delisting (announced at t1's close, at
    /// 5h, exit lead 2h): the t0 sell opens short 1 (fills t1's open);
    /// the t2 sell would add to it → refused; from 3h the engine buys the
    /// short back (t3's close → fills at t4's open, 100): flat, wallet
    /// whole (no fees, flat prices).
    #[tokio::test]
    async fn a_delisting_closes_a_perpetual_short_and_refuses_adding_to_it() {
        use bot_core::margin::{Bracket, Brackets, MarginMode};
        use bot_core::{AssetClass, Instrument, InstrumentSpec, MarketEvent, Side, Timestamp};
        use exchange::paper::PerpTerms;
        use std::collections::HashMap;
        let hour = crate::test_support::HOUR;
        let perp = Instrument::new(InstrumentSpec {
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let id = perp.id.clone();
        let brackets = Brackets::new(vec![Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(dec!(1000000)),
            max_leverage: 20,
            rate: dec!(0.004),
            cum: dec!(0),
        }])
        .unwrap();
        let provider = BacktestProvider::new(
            vec![perp],
            (0..6)
                .map(|i| candle(&id, i, ["100", "100", "100", "100"]))
                .collect(),
            PaperConfig {
                fees: RateFee::new(dec!(0), dec!(0), FeeAsset::Quote).unwrap(),
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        )
        .with_perp(crate::PerpSetup {
            terms: HashMap::from([(
                id.clone(),
                PerpTerms {
                    leverage: 5,
                    mode: MarginMode::Isolated,
                    brackets,
                },
            )]),
            clearance: dec!(0.0125),
        })
        .with_events(vec![MarketEvent::Delisting {
            instrument: id.clone(),
            at: Some(Timestamp::from_millis(5 * hour)),
            ts: Timestamp::from_millis(2 * hour - 1),
        }]);
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            delist_exit: std::time::Duration::from_secs(2 * 3600),
            ..Default::default()
        };
        let run = engine::run(&provider, &mut Shorter, Params::default(), &config)
            .await
            .unwrap();
        let fills: Vec<(Side, i64)> = run
            .fills
            .iter()
            .map(|f| (f.side, f.ts.as_millis() / hour))
            .collect();
        assert_eq!(fills, vec![(Side::Sell, 1), (Side::Buy, 4)]);
        assert_eq!(run.rejections.len(), 1);
        assert!(
            run.rejections[0].reason.contains("reduce"),
            "{}",
            run.rejections[0].reason
        );
        let r = Report::compute_full(
            &run,
            provider.candles(),
            provider.events(),
            provider.instruments(),
            provider.initial_cash(),
            Interval::H1,
            None,
        );
        assert_eq!(r.final_equity, Notional::new(dec!(1000)));
    }

    /// sma_long_short (SMA 3, notional 1,000) on a perpetual, isolated 2x,
    /// wallet 1,000, no fees or slippage; each bar opens at the previous
    /// close. Closes 100 100 100 110 90 80 120 120:
    /// - t3 (110 > SMA 103.33): buy 9.0909 → t4's open 110, margin 499.9995.
    /// - t4 (90 < SMA 100): flip, first half: close the 9.0909 reduce-only
    ///   → t5's open 90: realized −9.0909 × 20 = −181.818, wallet 818.182.
    /// - t5 (80 < SMA 93.33), flat: short 1000/80 = 12.5 → t6's open 80,
    ///   margin 500.
    /// - t6's high 120 crosses the short's liquidation price (500 + 12.5 ·
    ///   80) / (12.5 · 1.004) = 119.5219 → closed at 119.53 (tick against
    ///   the trader): loss 12.5 · 39.53 = 494.125, clearance fee 18.68
    ///   capped at the 5.875 left: the whole 500 margin. Wallet 318.182.
    /// - t6 and t7 (120 > SMA): long 8.33333 needs 499.9998 margin >
    ///   318.182 → refused twice.
    /// Final equity 318.182, realized −681.818. The old one-order flip was
    /// refused here and left a losing short open (ended 484.8487): lucky
    /// in this path, but a refused exit is the failure the change removes.
    #[tokio::test]
    async fn sma_long_short_trades_both_sides_of_a_perpetual() {
        use bot_core::margin::{Bracket, Brackets, MarginMode};
        use bot_core::{AssetClass, Decimal, Instrument, InstrumentSpec, Side};
        use exchange::paper::PerpTerms;
        use std::collections::HashMap;
        let perp = Instrument::new(InstrumentSpec {
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let id = perp.id.clone();
        let closes = ["100", "100", "100", "110", "90", "80", "120", "120"];
        let mut prev = "100";
        let mut bars = Vec::new();
        for (i, c) in closes.iter().enumerate() {
            let (o, cl): (Decimal, Decimal) = (prev.parse().unwrap(), c.parse().unwrap());
            let (h, l) = (o.max(cl).to_string(), o.min(cl).to_string());
            bars.push(candle(&id, i as i64, [prev, &h, &l, c]));
            prev = c;
        }
        let brackets = Brackets::new(vec![Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(dec!(1000000)),
            max_leverage: 20,
            rate: dec!(0.004),
            cum: dec!(0),
        }])
        .unwrap();
        let provider = BacktestProvider::new(
            vec![perp],
            bars,
            PaperConfig {
                fees: RateFee::new(dec!(0), dec!(0), FeeAsset::Quote).unwrap(),
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        )
        .with_perp(crate::PerpSetup {
            terms: HashMap::from([(
                id.clone(),
                PerpTerms {
                    leverage: 2,
                    mode: MarginMode::Isolated,
                    brackets,
                },
            )]),
            clearance: dec!(0.0125),
        });
        let params = Params::from_toml(
            "coin = \"binance:BTCUSDT\"\nsma_period = 3\nnotional = \"1000\"\nfee = \"0\"\nslippage_bps = \"0\"",
        )
        .unwrap();
        let mut s = strategy::build("sma_long_short", &params).unwrap();
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            ..Default::default()
        };
        let run = engine::run(&provider, s.as_mut(), params, &config)
            .await
            .unwrap();
        let fills: Vec<(Side, Decimal, Decimal)> = run
            .fills
            .iter()
            .map(|f| (f.side, f.qty.get(), f.price.get()))
            .collect();
        assert_eq!(
            fills,
            vec![
                (Side::Buy, dec!(9.0909), dec!(110)),
                (Side::Sell, dec!(9.0909), dec!(90)),
                (Side::Sell, dec!(12.5), dec!(80)),
                (Side::Buy, dec!(12.5), dec!(119.53)),
            ]
        );
        assert_eq!(run.rejections.len(), 2);
        assert!(run.rejections.iter().all(|r| r.reason.contains("margin")));
        let r = Report::compute_full(
            &run,
            provider.candles(),
            provider.events(),
            provider.instruments(),
            provider.initial_cash(),
            Interval::H1,
            None,
        );
        assert_eq!(r.final_equity, Notional::new(dec!(318.182)));
        assert_eq!(r.realized_pnl, Notional::new(dec!(-681.818)));
        // Peak 1,000 → trough 318.182.
        assert_eq!(r.max_drawdown, dec!(0.681818));
    }

    /// GTC limit buy 1 @ 100 and an IOC limit buy at t0's close.
    struct LimitBuyer;

    impl Strategy for LimitBuyer {
        fn name(&self) -> &str {
            "limit_buyer"
        }
        fn on_candle(
            &mut self,
            _ctx: &mut strategy::Context,
            c: &bot_core::Candle,
        ) -> Vec<bot_core::Signal> {
            if c.open_time.as_millis() != 0 {
                return vec![];
            }
            let limit = |tif| bot_core::Signal::Submit {
                instrument: c.instrument.clone(),
                side: bot_core::Side::Buy,
                size: bot_core::Size::Base(bot_core::Qty::new(dec!(1))),
                kind: bot_core::OrderKind::Limit {
                    price: bot_core::Price::new(dec!(100)),
                    tif,
                },
            };
            vec![
                limit(bot_core::TimeInForce::Gtc),
                limit(bot_core::TimeInForce::Ioc),
            ]
        }
    }

    /// eToro's model: t1 opens 101 and only touches 100 (low = 100) → the
    /// GTC limit fills at 100 + 10 bps = 100.10, worse than its limit
    /// (market-if-touched; the default model would wait for a
    /// trade-through and never fill worse than 100). The IOC limit is
    /// refused at submit, as eToro takes none.
    #[tokio::test]
    async fn etoro_limits_fill_on_a_touch_and_ioc_is_refused() {
        let id = btcusdt().id.clone();
        let provider = BacktestProvider::new(
            vec![btcusdt()],
            vec![
                candle(&id, 0, ["101", "101", "101", "101"]),
                candle(&id, 1, ["101", "101", "100", "100.5"]),
            ],
            PaperConfig {
                fees: RateFee::new(dec!(0), dec!(0), FeeAsset::Quote).unwrap(),
                slippage: BpsSlippage::new(dec!(10)).unwrap(),
                initial_cash: Notional::new(dec!(1000)),
            },
        )
        .with_touch_fills();
        let config = EngineConfig {
            whitelist: vec![id],
            history_len: 10,
            ..Default::default()
        };
        let run = engine::run(&provider, &mut LimitBuyer, Params::default(), &config)
            .await
            .unwrap();
        assert_eq!(run.fills.len(), 1);
        assert_eq!(run.fills[0].price.get(), dec!(100.10));
        assert_eq!(run.rejections.len(), 1);
        assert!(
            run.rejections[0].reason.contains("immediate-or-cancel"),
            "{}",
            run.rejections[0].reason
        );
    }

    /// Spot and perp together, two wallets of 1,000, no fees, 1x
    /// isolated. Bar 0 (closes 100) buys 1 spot and shorts 1 perp → both
    /// fill at bar 1's open 100 (spot cash 900; margin 100). Funding 1% at
    /// mark 100 just after bar 2 opens: the short receives 1 · 100 · 0.01
    /// = 1. Bar 2 closes 110 → both closed at bar 3's open 110: spot
    /// +110 → cash 1,010; the short loses 10 → wallet 1,000 + 1 − 10 = 991.
    /// Final equity 2,001 = 2,000 + the funding; both flat.
    #[tokio::test]
    async fn a_hedge_trades_spot_and_perp_from_two_wallets() {
        use bot_core::margin::{Bracket, Brackets, MarginMode};
        use bot_core::{
            AssetClass, Instrument, InstrumentSpec, OrderKind, Price, Qty, Side, Signal, Size,
        };
        use exchange::paper::PerpTerms;
        use std::collections::HashMap;
        use strategy::{Context, Strategy};
        let spot = btcusdt();
        let perp = Instrument::new(InstrumentSpec {
            id: "binance-usdm:BTCUSDT".parse().unwrap(),
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let mut bars = Vec::new();
        for (i, (o, c)) in [
            ("100", "100"),
            ("100", "100"),
            ("100", "110"),
            ("110", "110"),
        ]
        .iter()
        .enumerate()
        {
            let h = if c > o { *c } else { *o };
            for id in [&spot.id, &perp.id] {
                bars.push(candle(id, i as i64, [o, h, o, c]));
            }
        }
        let funding = bot_core::MarketEvent::Funding {
            instrument: perp.id.clone(),
            rate: dec!(0.01),
            mark: Price::new(dec!(100)),
            ts: bot_core::Timestamp::from_millis(2 * crate::test_support::HOUR + 1),
        };
        let brackets = Brackets::new(vec![Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(dec!(1000000)),
            max_leverage: 20,
            rate: dec!(0.004),
            cum: dec!(0),
        }])
        .unwrap();
        let zero = RateFee::new(dec!(0), dec!(0), FeeAsset::Quote).unwrap();
        let provider = BacktestProvider::new(
            vec![spot.clone(), perp.clone()],
            bars,
            PaperConfig {
                fees: zero,
                slippage: BpsSlippage::zero(),
                initial_cash: Notional::new(dec!(1000)),
            },
        )
        .with_hedge(crate::HedgeSetup {
            perp: crate::PerpSetup {
                terms: HashMap::from([(
                    perp.id.clone(),
                    PerpTerms {
                        leverage: 1,
                        mode: MarginMode::Isolated,
                        brackets,
                    },
                )]),
                clearance: dec!(0.0125),
            },
            wallet: Notional::new(dec!(1000)),
            fees: zero,
        })
        .with_events(vec![funding]);

        /// Opens the hedge on bar 0's perp candle, closes it on bar 2's.
        struct Hedger(bot_core::InstrumentId, bot_core::InstrumentId);
        impl Strategy for Hedger {
            fn name(&self) -> &str {
                "hedger"
            }
            fn on_candle(&mut self, _: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
                if c.instrument != self.1 {
                    return vec![];
                }
                let one = Qty::new(dec!(1));
                match c.open_time.as_millis() / crate::test_support::HOUR {
                    0 => vec![
                        Signal::Submit {
                            instrument: self.0.clone(),
                            side: Side::Buy,
                            size: Size::Base(one),
                            kind: OrderKind::Market,
                        },
                        Signal::Submit {
                            instrument: self.1.clone(),
                            side: Side::Sell,
                            size: Size::Base(one),
                            kind: OrderKind::Market,
                        },
                    ],
                    2 => vec![
                        Signal::Submit {
                            instrument: self.0.clone(),
                            side: Side::Sell,
                            size: Size::Base(one),
                            kind: OrderKind::Market,
                        },
                        Signal::Reduce {
                            instrument: self.1.clone(),
                            side: Side::Buy,
                            qty: one,
                            kind: OrderKind::Market,
                        },
                    ],
                    _ => vec![],
                }
            }
        }
        let mut s = Hedger(spot.id.clone(), perp.id.clone());
        let config = EngineConfig {
            whitelist: vec![spot.id.clone(), perp.id.clone()],
            history_len: 10,
            ..Default::default()
        };
        let report = run_backtest(&provider, &mut s, Params::default(), &config, Interval::H1)
            .await
            .unwrap();
        assert_eq!(report.initial_cash, Notional::new(dec!(2000)));
        assert_eq!(report.fills, 4);
        assert_eq!(report.funding, Notional::new(dec!(1)));
        assert_eq!(report.final_equity, Notional::new(dec!(2001)));
        assert_eq!(report.rejected_signals, 0);
    }
}
