use crate::EngineError;
use crate::risk;
use bot_core::{Fill, InstrumentId, MarketEvent, OrderId, Position, Signal, Timestamp};
use exchange::{Broker, InstrumentQuery, Provider};
use strategy::{Context, Params, Strategy, driver};

/// Run settings independent of venue and strategy.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// The only instruments the run may trade.
    pub whitelist: Vec<InstrumentId>,
    /// Candles kept per instrument in the strategy's rolling history.
    pub history_len: usize,
}

/// A signal that didn't reach the broker, and why. Rejections don't stop a
/// run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub at: Option<Timestamp>,
    pub signal: Signal,
    pub reason: String,
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    pub events: u64,
    /// Every fill, in execution order.
    pub fills: Vec<Fill>,
    pub rejections: Vec<Rejection>,
    /// Final position per whitelisted instrument, in whitelist order.
    pub positions: Vec<Position>,
}

/// Runs `strategy` against `provider` until the feed ends.
///
/// Each event goes through the same steps, in an order that is the core of
/// the no-lookahead guarantee (invariant 2):
/// 1. The feed must not go backwards in time.
/// 2. The broker settles pending orders against the event. Orders were
///    decided on earlier events, so they fill no earlier than this one.
/// 3. The context's open orders are refreshed from the broker. Each fill
///    updates the context's position and reaches `on_fill`,
///    before the event joins the history: a fill at a candle's open must
///    not see that candle's close.
/// 4. The event joins the history and reaches `on_event`.
/// 5. Signals from `on_fill` and `on_event` go through risk to the broker,
///    stamped with this event's time.
///
/// Backtest and live call this same function; only `provider` differs.
pub async fn run<P: Provider + ?Sized>(
    provider: &P,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
) -> Result<RunReport, EngineError> {
    let instruments = provider
        .resolve(&InstrumentQuery::Ids(config.whitelist.clone()))
        .await?;
    if let Some(i) = instruments
        .iter()
        .find(|i| !provider.asset_classes().contains(&i.class))
    {
        return Err(EngineError::UnsupportedAssetClass(i.id.clone()));
    }
    let mut feed = provider.market_data().await?;
    let mut broker = provider.broker().await?;
    let mut ctx = driver::new(instruments, params, config.history_len);
    let mut run = Run {
        broker: broker.as_mut(),
        next_order: 1,
        report: RunReport::default(),
    };

    strategy.on_start(&mut ctx);
    let signals = driver::take_signals(&mut ctx);
    run.route(&ctx, signals).await;

    let mut last: Option<Timestamp> = None;
    while let Some(event) = feed.next().await? {
        let ts = event.ts();
        if let Some(prev) = last.filter(|prev| ts < *prev) {
            return Err(EngineError::OutOfOrder {
                last: prev,
                got: ts,
            });
        }
        last = Some(ts);
        run.report.events += 1;

        let fills = run.broker.settle(&event).await?;
        driver::set_open_orders(&mut ctx, run.broker.open_orders());
        for fill in fills {
            driver::set_position(&mut ctx, run.broker.position(&fill.instrument));
            strategy.on_fill(&mut ctx, &fill);
            run.report.fills.push(fill);
        }

        driver::set_now(&mut ctx, ts);
        if let MarketEvent::Candle(c) = &event {
            driver::push_candle(&mut ctx, c.clone());
        }
        let returned = strategy.on_event(&mut ctx, &event);
        let mut signals = driver::take_signals(&mut ctx);
        signals.extend(returned);
        run.route(&ctx, signals).await;
    }

    run.report.positions = config
        .whitelist
        .iter()
        .map(|id| run.broker.position(id))
        .collect();
    Ok(run.report)
}

struct Run<'a> {
    broker: &'a mut dyn Broker,
    next_order: u64,
    report: RunReport,
}

impl Run<'_> {
    /// Deterministic ids, so two runs over the same data produce identical
    /// orders.
    fn mint_id(&mut self) -> OrderId {
        let id = OrderId::new(format!("o-{}", self.next_order));
        self.next_order += 1;
        // "o-" plus a u64 is at most 22 chars of [a-z0-9-]: always valid.
        id.unwrap_or_else(|_| unreachable!("generated order id is valid"))
    }

    async fn route(&mut self, ctx: &Context, signals: Vec<Signal>) {
        for signal in signals {
            let outcome = match &signal {
                Signal::Cancel(id) => self
                    .broker
                    .cancel(id.clone())
                    .await
                    .map_err(|e| e.to_string()),
                Signal::Submit {
                    instrument,
                    side,
                    size,
                    kind,
                } => {
                    let reference = ctx.last_candle(instrument).map(|c| c.close);
                    let now = ctx.now().unwrap_or(Timestamp::from_millis(i64::MIN));
                    let id = self.mint_id();
                    match risk::to_order(
                        id,
                        ctx.instrument(instrument),
                        *side,
                        *size,
                        *kind,
                        reference,
                        now,
                    ) {
                        Ok(order) => self
                            .broker
                            .submit(order)
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string()),
                        Err(r) => Err(r.to_string()),
                    }
                }
            };
            if let Err(reason) = outcome {
                tracing::debug!(%reason, "signal rejected");
                self.report.rejections.push(Rejection {
                    at: ctx.now(),
                    signal,
                    reason,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{HOUR, TestProvider, btcusdt, candle};
    use bot_core::{Fee, Liquidity, Notional, Price, Qty};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn config() -> EngineConfig {
        EngineConfig {
            whitelist: vec![btcusdt().id.clone()],
            history_len: 10,
        }
    }

    fn buy_and_hold(quote: &str) -> (Box<dyn Strategy>, Params) {
        let params = Params::from_toml(&format!(
            "instrument = \"binance:BTCUSDT\"\nquote_amount = \"{quote}\""
        ))
        .unwrap();
        (strategy::build("buy_and_hold", &params).unwrap(), params)
    }

    /// c0 decides the buy at close 100; c1 opens at 101.
    fn three_candles() -> Vec<MarketEvent> {
        vec![
            MarketEvent::Candle(candle(0, ["100", "101", "99", "100"])),
            MarketEvent::Candle(candle(1, ["101", "103", "100", "102"])),
            MarketEvent::Candle(candle(2, ["102", "104", "101", "103"])),
        ]
    }

    async fn run_with(slippage_bps: Decimal, fee_rate: Decimal) -> RunReport {
        let provider = TestProvider::new(three_candles(), slippage_bps, fee_rate);
        let (mut s, params) = buy_and_hold("1000");
        run(&provider, s.as_mut(), params, &config()).await.unwrap()
    }

    /// buy_and_hold, 1000 USDT budget, 0.1% taker fee in base, no slippage.
    /// Expected (confirmed by owner, 2026-10-22):
    ///   fill  = c1 open = 101, ts = c1 open_time
    ///   qty   = 1000 / 101 = 9.900990.. -> lot 0.00001 -> 9.90099 BTC
    ///   spend = 9.90099 * 101 = 999.99999 USDT (<= 1000)
    ///   fee   = 9.90099 * 0.001 = 0.00990099 BTC
    ///   held  = 9.90099 - 0.00990099 = 9.89108901 BTC, basis 999.99999
    #[tokio::test]
    async fn market_buy_fills_at_next_open() {
        let r = run_with(dec!(0), dec!(0.001)).await;
        assert_eq!(r.events, 3);
        assert!(r.rejections.is_empty());
        assert_eq!(r.fills.len(), 1, "buys once, then holds");
        let f = &r.fills[0];
        assert_eq!(f.price, Price::new(dec!(101)));
        assert_eq!(f.ts, Timestamp::from_millis(HOUR));
        assert_eq!(f.qty, Qty::new(dec!(9.90099)));
        assert_eq!(f.fee, Fee::Base(Qty::new(dec!(0.00990099))));
        assert_eq!(f.liquidity, Liquidity::Taker);

        let pos = &r.positions[0];
        assert_eq!(pos.qty(), Qty::new(dec!(9.89108901)));
        assert_eq!(pos.cost_basis(), Notional::new(dec!(999.99999)));
        assert!(pos.cost_basis() <= Notional::new(dec!(1000)));
        assert_eq!(pos.realized_pnl(), Notional::ZERO);
    }

    /// Same with 10 bps slippage (confirmed by owner, 2026-10-22):
    ///   price = 101 * 1.001 = 101.101 -> up to tick 101.11
    ///   qty   = 1000 / 101.11 = 9.890218.. -> 9.89021 BTC
    ///   spend = 9.89021 * 101.11 = 999.9991331 USDT (one more lot: 1000.0001442)
    ///   held  = 9.89021 - 0.00989021 = 9.88031979 BTC
    #[tokio::test]
    async fn slippage_moves_fill_against_buyer() {
        let r = run_with(dec!(10), dec!(0.001)).await;
        assert_eq!(r.fills[0].price, Price::new(dec!(101.11)));
        assert_eq!(r.fills[0].qty, Qty::new(dec!(9.89021)));
        assert_eq!(r.positions[0].qty(), Qty::new(dec!(9.88031979)));
        assert_eq!(
            r.positions[0].cost_basis(),
            Notional::new(dec!(999.9991331))
        );
    }

    #[tokio::test]
    async fn no_fill_without_a_next_candle() {
        let provider = TestProvider::new(three_candles()[..1].to_vec(), dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert!(r.fills.is_empty());
        assert!(r.positions[0].is_flat());
    }

    #[tokio::test]
    async fn too_small_buy_is_rejected_not_fatal() {
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("4");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert_eq!(r.events, 3);
        assert!(r.fills.is_empty());
        assert_eq!(r.rejections.len(), 1);
        assert_eq!(r.rejections[0].reason, "notional below instrument minimum");
    }

    #[tokio::test]
    async fn feed_going_backwards_stops_the_run() {
        let mut events = three_candles();
        events.swap(1, 2);
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let err = run(&provider, s.as_mut(), params, &config())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::OutOfOrder { .. }));
    }

    #[tokio::test]
    async fn refuses_provider_without_the_asset_class() {
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.classes = vec![bot_core::AssetClass::UsEquity];
        let (mut s, params) = buy_and_hold("1000");
        let err = run(&provider, s.as_mut(), params, &config())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::UnsupportedAssetClass(_)));
    }

    /// The strategy's view must match the broker after the fill, and the
    /// fill must reach on_fill before the filling candle joins history.
    #[tokio::test]
    async fn on_fill_runs_before_the_filling_candle_is_visible() {
        struct Probe {
            inner: Box<dyn Strategy>,
            seen: Vec<(usize, Qty)>,
        }
        impl Strategy for Probe {
            fn name(&self) -> &str {
                "probe"
            }
            fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal> {
                self.inner.on_event(ctx, e)
            }
            fn on_fill(&mut self, ctx: &mut Context, f: &Fill) {
                let id = &f.instrument;
                self.seen
                    .push((ctx.history(id).len(), ctx.position(id).qty()));
            }
        }
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0.001));
        let (inner, params) = buy_and_hold("1000");
        let mut probe = Probe {
            inner,
            seen: Vec::new(),
        };
        run(&provider, &mut probe, params, &config()).await.unwrap();
        // Fill at c1's open: only c0 in history; position already updated.
        assert_eq!(probe.seen, vec![(1, Qty::new(dec!(9.89108901)))]);
    }

    /// A strategy sees its resting order's id through the context and can
    /// cancel it; the next settle shows it gone.
    #[tokio::test]
    async fn strategy_sees_open_orders_and_cancels_by_id() {
        #[derive(Default)]
        struct Canceller {
            seen: Vec<Vec<String>>,
        }
        impl Strategy for Canceller {
            fn name(&self) -> &str {
                "canceller"
            }
            fn on_candle(&mut self, ctx: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
                let open: Vec<_> = ctx
                    .open_orders(&c.instrument)
                    .map(|o| o.id.clone())
                    .collect();
                self.seen
                    .push(open.iter().map(|id| id.to_string()).collect());
                match self.seen.len() {
                    // Rest a buy far below the market: never fills.
                    1 => vec![Signal::Submit {
                        instrument: c.instrument.clone(),
                        side: bot_core::Side::Buy,
                        size: bot_core::Size::Base(Qty::new(dec!(1))),
                        kind: bot_core::OrderKind::Limit {
                            price: Price::new(dec!(50)),
                            tif: bot_core::TimeInForce::Gtc,
                        },
                    }],
                    2 => open.into_iter().map(Signal::Cancel).collect(),
                    _ => Vec::new(),
                }
            }
        }
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let mut s = Canceller::default();
        let r = run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(s.seen, vec![vec![], vec!["o-1".to_owned()], vec![]]);
        assert!(r.rejections.is_empty());
        assert!(r.fills.is_empty());
    }
}
