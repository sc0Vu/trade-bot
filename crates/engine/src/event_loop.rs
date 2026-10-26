use crate::EngineError;
use crate::metrics as m;
use crate::risk;
use bot_core::{
    Candle, Fill, InstrumentId, MarketEvent, Order, OrderId, Position, Signal, Timestamp,
};
use exchange::{Broker, ExchangeError, InstrumentQuery, Provider};
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

/// What a live run looks like after one event: enough for a status page,
/// an API or metrics, without a handle on the broker or the strategy.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Snapshot {
    /// Time of the latest event (a candle's close time).
    pub at: Option<Timestamp>,
    pub events: u64,
    pub fills: usize,
    pub rejections: usize,
    /// Broker position per whitelisted instrument, in whitelist order.
    pub positions: Vec<Position>,
    /// The broker's working orders.
    pub open_orders: Vec<Order>,
    /// Latest closed candle per whitelisted instrument that has one.
    pub last_candles: Vec<Candle>,
    /// The strategy's [`Strategy::save_state`] after this event.
    pub strategy_state: Option<serde_json::Value>,
}

/// Watches a run from outside. Called after every event has been fully
/// handled (settled, decided, routed); it must return quickly, since the
/// loop waits for it. It can't change the run.
pub trait Observer: Send {
    fn on_snapshot(&mut self, snapshot: &Snapshot);
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
/// Before `on_start`, the context gets the broker's position for every
/// whitelisted instrument (flat in a backtest; a live broker's stored
/// holdings).
///
/// Backtest and live call this same function; only `provider` differs.
pub async fn run<P: Provider + ?Sized>(
    provider: &P,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
) -> Result<RunReport, EngineError> {
    run_inner(provider, strategy, params, config, None, None).await
}

/// [`run`], handing `observer` a [`Snapshot`] after every event, and
/// restoring the strategy from `resume` (a previous run's
/// [`Snapshot::strategy_state`]) after `on_start`. A state the strategy
/// refuses is logged and the strategy runs fresh. The loop is otherwise
/// identical, so what is observed is exactly what [`run`] does.
pub async fn run_observed<P: Provider + ?Sized>(
    provider: &P,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
    observer: &mut dyn Observer,
    resume: Option<serde_json::Value>,
) -> Result<RunReport, EngineError> {
    run_inner(provider, strategy, params, config, Some(observer), resume).await
}

async fn run_inner<P: Provider + ?Sized>(
    provider: &P,
    strategy: &mut dyn Strategy,
    params: Params,
    config: &EngineConfig,
    mut observer: Option<&mut dyn Observer>,
    resume: Option<serde_json::Value>,
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
    // A live broker may start from holdings an earlier run left; the
    // strategy must see them from `on_start` on.
    for id in &config.whitelist {
        driver::set_position(&mut ctx, broker.position(id));
    }
    let mut run = Run {
        broker: broker.as_mut(),
        next_order: 1,
        report: RunReport::default(),
    };

    strategy.on_start(&mut ctx);
    if let Some(state) = resume {
        match strategy.restore_state(state, &ctx) {
            Ok(()) => tracing::info!("strategy state restored"),
            Err(e) => tracing::warn!(error = %e, "saved strategy state discarded; starting fresh"),
        }
    }
    let signals = driver::take_signals(&mut ctx);
    run.route(&mut ctx, strategy, signals).await?;
    // Fills made while the bot was down reach `on_fill` with the first
    // event the strategy decides on, once its history has warmed up (a
    // follow-up order may need a last close).
    let mut recovered = run.broker.take_recovered_fills();

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
        metrics::counter!(m::EVENTS).increment(1);
        if let MarketEvent::Candle(c) | MarketEvent::Backfill(c) = &event {
            // Seconds as f64 for the exporter only; never read back.
            metrics::gauge!(m::LAST_CANDLE, "instrument" => c.instrument.to_string())
                .set(c.close_time.as_millis() as f64 / 1000.0);
        }

        let mut fills = run.broker.settle(&event).await?;
        if !recovered.is_empty() && !matches!(event, MarketEvent::Backfill(_)) {
            let mut late = std::mem::take(&mut recovered);
            late.append(&mut fills);
            fills = late;
        }
        driver::set_open_orders(&mut ctx, run.broker.open_orders());
        driver::set_cancelling_orders(&mut ctx, run.broker.cancelling_orders());
        for fill in fills {
            metrics::counter!(
                m::FILLS,
                "instrument" => fill.instrument.to_string(),
                "side" => side_label(fill.side),
                "liquidity" => match fill.liquidity {
                    bot_core::Liquidity::Maker => "maker",
                    bot_core::Liquidity::Taker => "taker",
                },
            )
            .increment(1);
            tracing::info!(order = %fill.order_id, instrument = %fill.instrument, side = ?fill.side, "fill");
            driver::set_position(&mut ctx, run.broker.position(&fill.instrument));
            strategy.on_fill(&mut ctx, &fill);
            run.report.fills.push(fill);
        }

        driver::set_now(&mut ctx, ts);
        // A backfilled candle is history like any other; whether it is a
        // decision is the strategy's `on_event` (the default says no).
        if let MarketEvent::Candle(c) | MarketEvent::Backfill(c) = &event {
            driver::push_candle(&mut ctx, c.clone());
        }
        let returned = strategy.on_event(&mut ctx, &event);
        let mut signals = driver::take_signals(&mut ctx);
        signals.extend(returned);
        run.route(&mut ctx, strategy, signals).await?;

        if let Some(o) = observer.as_deref_mut() {
            o.on_snapshot(&Snapshot {
                at: Some(ts),
                events: run.report.events,
                fills: run.report.fills.len(),
                rejections: run.report.rejections.len(),
                positions: config
                    .whitelist
                    .iter()
                    .map(|id| run.broker.position(id))
                    .collect(),
                open_orders: run.broker.open_orders(),
                last_candles: config
                    .whitelist
                    .iter()
                    .filter_map(|id| ctx.last_candle(id).cloned())
                    .collect(),
                strategy_state: strategy.save_state(),
            });
        }
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

    /// Sends each signal through risk to the broker. A refused order is
    /// recorded and the loop goes on; any other broker error stops the run
    /// (see [`ExchangeError::is_order_rejection`]).
    /// Sends each signal through risk to the broker and tells the strategy
    /// what became of it (`on_order` / `on_rejected`). Signals the strategy
    /// submits from those hooks wait in `ctx` for the next event.
    async fn route(
        &mut self,
        ctx: &mut Context,
        strategy: &mut dyn Strategy,
        signals: Vec<Signal>,
    ) -> Result<(), EngineError> {
        for signal in signals {
            let outcome = match &signal {
                Signal::Cancel(id) => match self.broker.cancel(id.clone()).await {
                    Ok(()) => {
                        metrics::counter!(m::CANCELS).increment(1);
                        tracing::info!(order = %id, "cancel requested");
                        Ok(())
                    }
                    Err(e) => Err(refusal(e)?),
                },
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
                        Ok(order) => {
                            let (id, instrument, side) =
                                (order.id.clone(), order.instrument.clone(), order.side);
                            match self.broker.submit(order.clone()).await {
                                Ok(_) => {
                                    metrics::counter!(
                                        m::ORDERS,
                                        "instrument" => instrument.to_string(),
                                        "side" => side_label(side),
                                    )
                                    .increment(1);
                                    tracing::info!(order = %id, %instrument, ?side, "order submitted");
                                    strategy.on_order(ctx, &order);
                                    Ok(())
                                }
                                Err(e) => Err(refusal(e)?),
                            }
                        }
                        Err(r) => {
                            metrics::counter!(m::REJECTED, "source" => "risk", "code" => "risk")
                                .increment(1);
                            Err(r.to_string())
                        }
                    }
                }
            };
            if let Err(reason) = outcome {
                tracing::info!(%reason, "signal rejected");
                strategy.on_rejected(ctx, &signal, &reason);
                self.report.rejections.push(Rejection {
                    at: ctx.now(),
                    signal,
                    reason,
                });
            }
        }
        Ok(())
    }
}

/// Turns a broker error into the reason a signal was refused, or into the
/// error that stops the run when it isn't a refusal of this one order.
fn refusal(e: ExchangeError) -> Result<String, EngineError> {
    if e.is_order_rejection() {
        let code = match &e {
            ExchangeError::Venue { code, .. } => code.to_string(),
            ExchangeError::UnknownOrder(_) => "unknown_order".to_owned(),
            ExchangeError::UnknownInstrument(_) => "unknown_instrument".to_owned(),
            _ => "refused".to_owned(),
        };
        metrics::counter!(m::REJECTED, "source" => "broker", "code" => code).increment(1);
        Ok(e.to_string())
    } else {
        Err(e.into())
    }
}

fn side_label(side: bot_core::Side) -> &'static str {
    match side {
        bot_core::Side::Buy => "buy",
        bot_core::Side::Sell => "sell",
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
            "coin = \"binance:BTCUSDT\"\nquote_amount = \"{quote}\"\n\
             fee = \"0.001\"\nslippage_bps = \"0\""
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
    async fn venue_refusing_one_order_is_not_fatal() {
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.submit_error = Some(-2010);
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert_eq!(r.events, 3);
        assert_eq!(r.rejections.len(), 1);
        assert_eq!(r.rejections[0].reason, "venue error -2010: test");
    }

    #[tokio::test]
    async fn bad_credentials_stop_the_run() {
        // Seen on testnet: -2015 was recorded as a rejection, and the run
        // went on for minutes looking healthy while no order could ever
        // be placed.
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.submit_error = Some(-2015);
        let (mut s, params) = buy_and_hold("1000");
        let err = run(&provider, s.as_mut(), params, &config())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            EngineError::Exchange(ExchangeError::Venue { code: -2015, .. })
        ));
    }

    #[tokio::test]
    async fn strategy_starts_with_the_brokers_positions() {
        struct Peek(Option<Position>);
        impl Strategy for Peek {
            fn name(&self) -> &str {
                "peek"
            }
            fn on_start(&mut self, ctx: &mut Context) {
                self.0 = Some(ctx.position(&btcusdt().id));
            }
        }
        let held = Position::from_parts(
            btcusdt().id.clone(),
            Qty::new(dec!(2)),
            Notional::new(dec!(200)),
            Notional::ZERO,
        )
        .unwrap();
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.prior = Some(held.clone());
        let mut s = Peek(None);
        run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(s.0, Some(held));
    }

    type Snap = Vec<(
        metrics_util::CompositeKey,
        Option<metrics::Unit>,
        Option<metrics::SharedString>,
        metrics_util::debugging::DebugValue,
    )>;

    /// The metric `name` whose labels include all of `labels`. Take `snap`
    /// once per check: each snapshot resets the recorder's counters.
    fn find<'a>(
        snap: &'a Snap,
        name: &str,
        labels: &[(&str, &str)],
    ) -> Option<&'a metrics_util::debugging::DebugValue> {
        snap.iter()
            .find(|(k, _, _, _)| {
                k.key().name() == name
                    && labels.iter().all(|(lk, lv)| {
                        k.key().labels().any(|l| l.key() == *lk && l.value() == *lv)
                    })
            })
            .map(|(_, _, _, v)| v)
    }

    fn counter(snap: &Snap, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
        match find(snap, name, labels)? {
            metrics_util::debugging::DebugValue::Counter(c) => Some(*c),
            _ => None,
        }
    }

    fn gauge(snap: &Snap, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        match find(snap, name, labels)? {
            metrics_util::debugging::DebugValue::Gauge(g) => Some(g.into_inner()),
            _ => None,
        }
    }

    /// One buy over three candles: 3 events, 1 accepted order, 1 taker
    /// fill, and the last-candle gauge at c2's close (10799.999 s).
    #[tokio::test]
    async fn records_events_orders_and_fills() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        run_with(dec!(0), dec!(0.001)).await;
        let snap = snapshotter.snapshot().into_vec();

        let btc = btcusdt().id.to_string();
        assert_eq!(counter(&snap, m::EVENTS, &[]), Some(3));
        assert_eq!(
            counter(&snap, m::ORDERS, &[("instrument", &btc), ("side", "buy")]),
            Some(1)
        );
        assert_eq!(counter(&snap, m::FILLS, &[("liquidity", "taker")]), Some(1));
        assert_eq!(
            gauge(&snap, m::LAST_CANDLE, &[("instrument", &btc)]),
            Some(10_799.999)
        );
    }

    #[tokio::test]
    async fn records_rejections_by_source() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // 4 USDT is under the 5 USDT minimum: risk refuses it.
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("4");
        run(&provider, s.as_mut(), params, &config()).await.unwrap();
        let snap = snapshotter.snapshot().into_vec();
        assert_eq!(counter(&snap, m::REJECTED, &[("source", "risk")]), Some(1));

        // The venue refusing one order is counted under its code.
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.submit_error = Some(-2010);
        let (mut s, params) = buy_and_hold("1000");
        run(&provider, s.as_mut(), params, &config()).await.unwrap();
        let snap = snapshotter.snapshot().into_vec();
        assert_eq!(
            counter(
                &snap,
                m::REJECTED,
                &[("source", "broker"), ("code", "-2010")]
            ),
            Some(1)
        );
    }

    /// One snapshot per event; the last shows the run's end state: 3
    /// events, the one fill, the held position and c2 as the last candle.
    #[tokio::test]
    async fn observer_sees_every_event() {
        struct Keep(Vec<Snapshot>);
        impl Observer for Keep {
            fn on_snapshot(&mut self, s: &Snapshot) {
                self.0.push(s.clone());
            }
        }
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0.001));
        let (mut s, params) = buy_and_hold("1000");
        let mut keep = Keep(Vec::new());
        let r = run_observed(&provider, s.as_mut(), params, &config(), &mut keep, None)
            .await
            .unwrap();
        assert_eq!(keep.0.len(), 3);
        let last = keep.0.last().unwrap();
        assert_eq!(last.events, 3);
        assert_eq!(last.fills, 1);
        assert_eq!(last.positions, r.positions);
        assert!(last.open_orders.is_empty());
        assert_eq!(last.last_candles, vec![three_candles_last()]);
        assert_eq!(last.at, Some(three_candles_last().close_time));
        // The first snapshot is after c0: the buy is decided, not filled.
        assert_eq!(keep.0[0].fills, 0);
        assert_eq!(keep.0[0].open_orders.len(), 1);
    }

    fn three_candles_last() -> bot_core::Candle {
        match three_candles().pop() {
            Some(MarketEvent::Candle(c)) => c,
            other => panic!("{other:?}"),
        }
    }

    /// Two backfilled bars then a live one: all three reach the history,
    /// only the live one is a decision (buy_and_hold buys on it, at c2's
    /// close, and nothing fills since the feed ends there).
    #[tokio::test]
    async fn backfilled_candles_are_history_not_decisions() {
        struct Peek(Vec<usize>, usize);
        impl Strategy for Peek {
            fn name(&self) -> &str {
                "peek"
            }
            fn on_candle(&mut self, ctx: &mut Context, _: &bot_core::Candle) -> Vec<Signal> {
                self.0.push(ctx.history(&btcusdt().id).len());
                self.1 += 1;
                Vec::new()
            }
        }
        let mut events = three_candles();
        for e in events.iter_mut().take(2) {
            if let MarketEvent::Candle(c) = e {
                *e = MarketEvent::Backfill(c.clone());
            }
        }
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let mut s = Peek(Vec::new(), 0);
        let r = run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(r.events, 3);
        assert_eq!(s.1, 1, "one decision");
        assert_eq!(s.0, vec![3], "made with all three bars in history");
    }

    /// Counts candles it decides on; saves and restores that count.
    struct Counter {
        seen: u64,
        fills: Vec<(u64, bot_core::OrderId)>,
        refuse: bool,
    }

    impl Strategy for Counter {
        fn name(&self) -> &str {
            "counter"
        }
        fn on_candle(&mut self, _: &mut Context, _: &bot_core::Candle) -> Vec<Signal> {
            self.seen += 1;
            Vec::new()
        }
        fn on_fill(&mut self, _: &mut Context, f: &Fill) {
            self.fills.push((self.seen, f.order_id.clone()));
        }
        fn save_state(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "v": 1, "seen": self.seen }))
        }
        fn restore_state(
            &mut self,
            state: serde_json::Value,
            _: &Context,
        ) -> Result<(), strategy::StrategyError> {
            if self.refuse {
                return Err(strategy::StrategyError::State {
                    strategy: "counter".into(),
                    reason: "test".into(),
                });
            }
            self.seen = state["seen"].as_u64().unwrap();
            Ok(())
        }
    }

    struct Keep(Vec<Snapshot>);
    impl Observer for Keep {
        fn on_snapshot(&mut self, s: &Snapshot) {
            self.0.push(s.clone());
        }
    }

    fn counting(refuse: bool) -> Counter {
        Counter {
            seen: 0,
            fills: Vec::new(),
            refuse,
        }
    }

    /// Resumed at 10, three candles later the saved state says 13; a
    /// refused state starts from 0 instead.
    #[tokio::test]
    async fn state_is_restored_and_saved_each_event() {
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let resume = Some(serde_json::json!({ "v": 1, "seen": 10 }));
        let mut s = counting(false);
        let mut keep = Keep(Vec::new());
        run_observed(
            &provider,
            &mut s,
            Params::default(),
            &config(),
            &mut keep,
            resume.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            keep.0.last().unwrap().strategy_state,
            Some(serde_json::json!({ "v": 1, "seen": 13 }))
        );

        let mut s = counting(true);
        let mut keep = Keep(Vec::new());
        run_observed(
            &provider,
            &mut s,
            Params::default(),
            &config(),
            &mut keep,
            resume,
        )
        .await
        .unwrap();
        assert_eq!(s.seen, 3, "refused state: fresh run");
    }

    /// A fill recovered at startup reaches on_fill with the first decision
    /// event (c2), not during the warm-up backfill (c0, c1).
    #[tokio::test]
    async fn recovered_fills_arrive_with_the_first_decision_event() {
        let mut events = three_candles();
        for e in events.iter_mut().take(2) {
            if let MarketEvent::Candle(c) = e {
                *e = MarketEvent::Backfill(c.clone());
            }
        }
        let mut provider = TestProvider::new(events, dec!(0), dec!(0));
        let recovered = bot_core::Fill::new(bot_core::FillSpec {
            order_id: bot_core::OrderId::new("tbold-o-7").unwrap(),
            instrument: btcusdt().id.clone(),
            side: bot_core::Side::Buy,
            qty: Qty::new(dec!(1)),
            price: Price::new(dec!(100)),
            fee: Fee::Quote(Notional::ZERO),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(1),
        })
        .unwrap();
        provider.recovered = vec![recovered];
        let mut s = counting(false);
        let r = run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        // Delivered before c2's decision (seen still 0), exactly once.
        assert_eq!(
            s.fills,
            vec![(0, bot_core::OrderId::new("tbold-o-7").unwrap())]
        );
        assert_eq!(r.fills.len(), 1);
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

    /// The strategy hears what became of each signal before any fill of it:
    /// c0 sends a 4 USDT buy (under the 5 USDT minimum: refused by risk)
    /// and a 1 BTC market buy, which becomes `o-2` (ids are minted per
    /// signal, refused ones included) and fills at c1's open under that id.
    /// A venue refusal (`-2010`) is reported the same way.
    #[tokio::test]
    async fn strategy_hears_placed_orders_and_refusals() {
        #[derive(Default)]
        struct Listener {
            log: Vec<String>,
        }
        impl Strategy for Listener {
            fn name(&self) -> &str {
                "listener"
            }
            fn on_candle(&mut self, _: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
                if !self.log.is_empty() {
                    return Vec::new();
                }
                self.log.push("decide".into());
                let buy = |size| Signal::Submit {
                    instrument: c.instrument.clone(),
                    side: bot_core::Side::Buy,
                    size,
                    kind: bot_core::OrderKind::Market,
                };
                vec![
                    buy(bot_core::Size::Quote(bot_core::Notional::new(dec!(4)))),
                    buy(bot_core::Size::Base(Qty::new(dec!(1)))),
                ]
            }
            fn on_order(&mut self, _: &mut Context, o: &Order) {
                self.log.push(format!("placed {}", o.id));
            }
            fn on_rejected(&mut self, _: &mut Context, _: &Signal, reason: &str) {
                self.log.push(format!("refused: {reason}"));
            }
            fn on_fill(&mut self, _: &mut Context, f: &Fill) {
                self.log.push(format!("fill {}", f.order_id));
            }
        }
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let mut s = Listener::default();
        run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(
            s.log,
            vec![
                "decide",
                "refused: notional below instrument minimum",
                "placed o-2",
                "fill o-2"
            ]
        );

        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.submit_error = Some(-2010);
        let mut s = Listener::default();
        run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(s.log[2], "refused: venue error -2010: test");
    }

    /// An order the venue hasn't confirmed cancelled yet leaves
    /// `open_orders` at once but shows in `cancelling_orders`, so a
    /// strategy can tell a fill may still come.
    #[tokio::test]
    async fn strategy_sees_orders_still_being_cancelled() {
        #[derive(Default)]
        struct Watcher {
            seen: Vec<(usize, Vec<String>)>,
        }
        impl Strategy for Watcher {
            fn name(&self) -> &str {
                "watcher"
            }
            fn on_candle(&mut self, ctx: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
                let open: Vec<_> = ctx
                    .open_orders(&c.instrument)
                    .map(|o| o.id.clone())
                    .collect();
                let cancelling = ctx
                    .cancelling_orders(&c.instrument)
                    .map(|o| o.id.to_string())
                    .collect();
                self.seen.push((open.len(), cancelling));
                match self.seen.len() {
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
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.slow_cancel = true;
        let mut s = Watcher::default();
        run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(
            s.seen,
            vec![(0, vec![]), (1, vec![]), (0, vec!["o-1".to_owned()])]
        );
    }
}
