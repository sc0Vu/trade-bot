use crate::EngineError;
use crate::metrics as m;
use crate::risk;
use bot_core::{
    Candle, Decimal, Fill, InstrumentId, MarketEvent, Notional, Order, OrderId, OrderKind,
    Position, Side, Signal, Size, Timestamp,
};
use exchange::{Broker, ExchangeError, InstrumentQuery, Provider};
use std::collections::HashSet;
use std::time::Duration;
use strategy::{Context, Params, Strategy, driver};

/// Run settings independent of venue and strategy. Build it with the
/// fields you set and `..Default::default()` for the rest, so a new
/// setting doesn't break every caller.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// The only instruments the run may trade.
    pub whitelist: Vec<InstrumentId>,
    /// Candles kept per instrument in the strategy's rolling history.
    pub history_len: usize,
    /// How long before a scheduled delisting (`MarketEvent::Delisting`) the
    /// engine itself cancels the strategy's orders on the instrument and
    /// sells the holding at market, whatever the strategy does. New buys
    /// are refused from the announcement on.
    pub delist_exit: Duration,
    /// Stop the strategy once the run's equity falls this far from its
    /// peak (see [`KillSwitch`]); `None`: never.
    pub kill_switch: Option<KillSwitch>,
}

/// A drawdown limit for the whole run, enforced by the engine whatever the
/// strategy does.
///
/// Equity = `capital` + the run's P&L: realized (fees and funding
/// included) plus each open position at its last close
/// ([`Instrument::notional`](bot_core::Instrument::notional) − cost
/// basis), over the whitelist, in their (one) quote. The peak starts at
/// `capital`. When equity falls to `peak · (1 − max_drawdown)` or below at
/// a candle, the strategy is muted for good (no hooks, no signals); the
/// engine cancels every working order and closes every position at
/// market, reduce-only, retrying each event like the delisting exit, and
/// once all is flat and nothing works the run ends with
/// [`RunReport::killed`] set. Same in a backtest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillSwitch {
    /// What the drawdown is measured against at the start (the money the
    /// strategy trades with).
    pub capital: Notional,
    /// Fraction of the peak (`0.25` = 25%), in (0, 1).
    pub max_drawdown: Decimal,
}

/// Default for [`EngineConfig::delist_exit`]: a day, enough for a few
/// tries if a sale is refused, well inside the notice Binance gives.
pub const DEFAULT_DELIST_EXIT: Duration = Duration::from_secs(24 * 3600);

impl Default for EngineConfig {
    /// No instruments; `history_len` 500 (the CLI's default);
    /// [`DEFAULT_DELIST_EXIT`].
    fn default() -> Self {
        Self {
            whitelist: Vec::new(),
            history_len: 500,
            delist_exit: DEFAULT_DELIST_EXIT,
            kill_switch: None,
        }
    }
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
    /// Why the kill switch stopped the run, if it did ([`KillSwitch`]):
    /// everything was closed and the feed's remaining events weren't read.
    pub killed: Option<String>,
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
    /// Fills settled at this event, in settle order: for an observer that
    /// keeps its own record (a paper run's API books). Not serialized:
    /// `fills` counts them all.
    #[serde(skip)]
    pub new_fills: Vec<bot_core::Fill>,
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
        exit_cancelled: HashSet::new(),
        exit_sells: HashSet::new(),
        peak: config.kill_switch.map(|k| k.capital),
        killed: None,
    };

    strategy.on_start(&mut ctx);
    if let Some(state) = resume {
        match strategy.restore_state(state, &ctx) {
            Ok(()) => tracing::info!("strategy state restored"),
            Err(e) => tracing::warn!(error = %e, "saved strategy state discarded; starting fresh"),
        }
    }
    check_fault(strategy)?;
    let signals = driver::take_signals(&mut ctx);
    run.route(&mut ctx, strategy, signals).await?;
    check_fault(strategy)?;
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
        let first_fill = run.report.fills.len();
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
            tracing::info!(order = %run.broker.client_order_id(&fill.order_id), instrument = %fill.instrument, side = ?fill.side, "fill");
            driver::set_position(&mut ctx, run.broker.position(&fill.instrument));
            if run.killed.is_none() {
                strategy.on_fill(&mut ctx, &fill);
            }
            run.report.fills.push(fill);
        }

        if let MarketEvent::Delisting { instrument, at, .. } = &event {
            match at {
                Some(at) => {
                    tracing::warn!(%instrument, at = %at, "delisting scheduled: no new buys; holding sold before")
                }
                None => tracing::info!(%instrument, "delisting withdrawn"),
            }
            driver::set_delisting(&mut ctx, instrument.clone(), *at);
        }
        if let MarketEvent::Funding { instrument, .. } = &event {
            // Funding moved realized P&L without a fill.
            driver::set_position(&mut ctx, run.broker.position(instrument));
        }
        if let MarketEvent::TradingStatus {
            instrument,
            trading,
            ..
        } = &event
        {
            if *trading {
                tracing::info!(%instrument, "trading resumed");
            } else {
                tracing::warn!(%instrument, "trading halted: no orders until it resumes");
            }
            driver::set_trading(&mut ctx, instrument.clone(), *trading);
        }
        if let MarketEvent::MarketSession {
            instrument, open, ..
        } = &event
        {
            if *open {
                tracing::info!(%instrument, "market open");
            } else {
                tracing::info!(%instrument, "market closed: orders wait for the open");
            }
            driver::set_session(&mut ctx, instrument.clone(), *open);
        }
        driver::set_now(&mut ctx, ts);
        // A backfilled candle is history like any other; whether it is a
        // decision is the strategy's `on_event` (the default says no).
        if let MarketEvent::Candle(c) | MarketEvent::Backfill(c) = &event {
            driver::push_candle(&mut ctx, c.clone());
        }
        if run.killed.is_none() {
            let returned = strategy.on_event(&mut ctx, &event);
            // Covers this event's `on_fill` calls too: nothing they or
            // `on_event` queued is routed from a strategy that failed.
            check_fault(strategy)?;
            let mut signals = driver::take_signals(&mut ctx);
            signals.extend(returned);
            run.route(&mut ctx, strategy, signals).await?;
            check_fault(strategy)?;
        }
        run.exit_delisting(&ctx, config).await?;
        if let (Some(ks), MarketEvent::Candle(_)) = (config.kill_switch, &event)
            && run.killed.is_none()
            && let Some(reason) = run.drawdown_breach(&ctx, config, ks)
        {
            tracing::error!(%reason, "kill switch: strategy stopped; closing every position");
            metrics::counter!(m::KILL_SWITCH).increment(1);
            run.killed = Some(reason);
        }
        if run.killed.is_some() && run.close_everything(&ctx, config).await? {
            run.report.killed = run.killed.take();
            break;
        }

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
                new_fills: run.report.fills[first_fill..].to_vec(),
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

/// Stops the run if the strategy reported a fault (`Strategy::fault`).
fn check_fault(strategy: &dyn Strategy) -> Result<(), EngineError> {
    match strategy.fault() {
        Some(reason) => Err(EngineError::StrategyFault(reason)),
        None => Ok(()),
    }
}

struct Run<'a> {
    broker: &'a mut dyn Broker,
    next_order: u64,
    report: RunReport,
    /// Orders the delisting exit already asked to cancel.
    exit_cancelled: HashSet<OrderId>,
    /// The delisting exit's own closing orders: waited for, never cancelled.
    exit_sells: HashSet<OrderId>,
    /// Highest equity seen (kill switch on), from its `capital`.
    peak: Option<Notional>,
    /// Set when the kill switch trips: the strategy is muted from then on.
    killed: Option<String>,
}

/// Whether a `side` order on `id` could open or add to a position rather
/// than only reduce one: a buy unless short; a sell when flat or short on
/// an instrument that can be shorted (a spot sell never opens anything).
/// A buy bigger than the short it covers is let through (a flip).
fn opens(ctx: &Context, id: &InstrumentId, side: Side) -> bool {
    let held = ctx.position(id).qty();
    match side {
        Side::Buy => !held.is_negative(),
        Side::Sell => !held.is_positive() && ctx.instrument(id).is_some_and(|i| i.allows_short()),
    }
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

    /// The engine's own exit from instruments being delisted: while `now`
    /// is within `config.delist_exit` before an instrument's delisting, cancel
    /// every order on it still working (once each), and when none is
    /// working or being cancelled, close the lot-rounded position at market
    /// (sell a long, buy back a short) if the venue would take it (dust
    /// stays). Runs after the strategy's
    /// signals on every event, so it retries until flat; the strategy
    /// hears the fills like any other, but these orders aren't its signals
    /// (no `on_order` / `on_rejected`). A refusal is recorded in the
    /// report and logged; other broker errors stop the run, as for any
    /// order.
    async fn exit_delisting(
        &mut self,
        ctx: &Context,
        config: &EngineConfig,
    ) -> Result<(), EngineError> {
        let Some(now) = ctx.now() else {
            return Ok(());
        };
        let lead = i64::try_from(config.delist_exit.as_millis()).unwrap_or(i64::MAX);
        for id in &config.whitelist {
            let Some(at) = ctx.delisting(id) else {
                continue;
            };
            // The window ends at the delisting: after it the venue refuses
            // every order, and what's left stays where it is.
            if now.as_millis() < at.as_millis().saturating_sub(lead) || now >= at {
                continue;
            }
            self.close_instrument(ctx, id, now, "delisting exit")
                .await?;
        }
        Ok(())
    }

    /// The run's equity for the kill switch (see [`KillSwitch`]); a reason
    /// when it is at or below `peak · (1 − max_drawdown)`, after raising
    /// the peak.
    fn drawdown_breach(
        &mut self,
        ctx: &Context,
        config: &EngineConfig,
        ks: KillSwitch,
    ) -> Option<String> {
        let pnl: Notional = config
            .whitelist
            .iter()
            .map(|id| {
                let p = self.broker.position(id);
                let open = match (ctx.instrument(id), ctx.last_candle(id)) {
                    (Some(inst), Some(c)) => inst.notional(p.qty(), c.close) - p.cost_basis(),
                    _ => Notional::ZERO,
                };
                p.realized_pnl() + open
            })
            .sum();
        let equity = ks.capital + pnl;
        let peak = self.peak.unwrap_or(ks.capital).max(equity);
        self.peak = Some(peak);
        let floor = peak.get() * (Decimal::ONE - ks.max_drawdown);
        (equity.get() <= floor).then(|| {
            format!(
                "equity {} fell to or below {}% under its peak {}",
                equity.get().round_dp(8),
                (ks.max_drawdown * Decimal::ONE_HUNDRED).normalize(),
                peak.get().round_dp(8)
            )
        })
    }

    /// The kill switch's exit: every whitelisted instrument closed as the
    /// delisting exit closes one. True once all are flat (or dust) with
    /// nothing working or being cancelled.
    async fn close_everything(
        &mut self,
        ctx: &Context,
        config: &EngineConfig,
    ) -> Result<bool, EngineError> {
        let Some(now) = ctx.now() else {
            return Ok(false);
        };
        let mut done = true;
        for id in &config.whitelist {
            done &= self.close_instrument(ctx, id, now, "kill switch").await?;
        }
        Ok(done)
    }

    /// Closes `id` for an engine exit (`why` names it in logs and
    /// rejections): cancel every working order on it (once each), and
    /// when none is working or being cancelled, close the lot-rounded
    /// position at market, reduce-only (sell a long, buy back a short), if
    /// the venue would take it (dust stays). The strategy hears the fills
    /// like any other, but these orders aren't its signals (no `on_order` /
    /// `on_rejected`). A refusal is recorded in the report and logged;
    /// other broker errors stop the run, as for any order. True when
    /// nothing is left to do: flat or dust, nothing working.
    async fn close_instrument(
        &mut self,
        ctx: &Context,
        id: &InstrumentId,
        now: Timestamp,
        why: &str,
    ) -> Result<bool, EngineError> {
        if ctx.halted(id) {
            return Ok(false); // nothing can be done until it resumes
        }
        let working: Vec<OrderId> = self
            .broker
            .open_orders()
            .into_iter()
            .filter(|o| o.instrument == *id)
            .map(|o| o.id.clone())
            .collect();
        if working.iter().any(|o| self.exit_sells.contains(o)) {
            return Ok(false); // our sell is working: wait for it
        }
        for oid in &working {
            if !self.exit_cancelled.insert(oid.clone()) {
                continue;
            }
            tracing::warn!(order = %self.broker.client_order_id(oid), instrument = %id, "engine exit: cancelling order");
            if let Err(e) = self.broker.cancel(oid.clone()).await {
                let reason = format!("{why}: {}", refusal(e)?);
                self.reject(now, Signal::Cancel(oid.clone()), reason);
            }
        }
        let cancelling = self
            .broker
            .cancelling_orders()
            .iter()
            .any(|o| o.instrument == *id);
        if !working.is_empty() || cancelling {
            return Ok(false); // sell once nothing else can fill
        }
        let (Some(inst), Some(last)) = (ctx.instrument(id), ctx.last_candle(id)) else {
            return Ok(false);
        };
        let held = self.broker.position(id).qty();
        let side = if held.is_negative() {
            Side::Buy
        } else {
            Side::Sell
        };
        let units = inst.round_qty(held.abs());
        if inst.check_market(units, last.close, side).is_err() {
            return Ok(true); // flat, or dust the venue won't take
        }
        // Reduce-only: the exit closes, it never opens or flips.
        let signal = Signal::Reduce {
            instrument: id.clone(),
            side,
            qty: units,
            kind: OrderKind::Market,
        };
        let oid = self.mint_id();
        let market = risk::MarketPrices {
            last_close: Some(last.close),
            average: None,
        };
        let outcome = match risk::to_order(
            oid,
            Some(inst),
            side,
            Size::Base(units),
            OrderKind::Market,
            market,
            now,
        )
        .and_then(|o| o.with_reduce_only().map_err(risk::RiskRejection::Invalid))
        {
            Ok(order) => {
                let oid = order.id.clone();
                match self.broker.submit(order).await {
                    Ok(_) => {
                        tracing::warn!(order = %self.broker.client_order_id(&oid), instrument = %id, side = ?side, "engine exit: closing the position");
                        self.exit_sells.insert(oid);
                        Ok(())
                    }
                    Err(e) => Err(refusal(e)?),
                }
            }
            Err(r) => Err(r.to_string()),
        };
        if let Err(reason) = outcome {
            self.reject(now, signal, format!("{why}: {reason}"));
        }
        Ok(false)
    }

    fn reject(&mut self, at: Timestamp, signal: Signal, reason: String) {
        tracing::warn!(%reason, "signal rejected");
        self.report.rejections.push(Rejection {
            at: Some(at),
            signal,
            reason,
        });
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
                        tracing::info!(order = %self.broker.client_order_id(id), "cancel requested");
                        Ok(())
                    }
                    Err(e) => Err(refusal(e)?),
                },
                Signal::Submit { instrument, .. } | Signal::Reduce { instrument, .. }
                    if ctx.halted(instrument) =>
                {
                    metrics::counter!(m::REJECTED, "source" => "risk", "code" => "halted")
                        .increment(1);
                    Err("instrument is halted: no orders until it resumes".to_owned())
                }
                // A reduce-only order can't open anything: never refused here.
                Signal::Submit {
                    instrument, side, ..
                } if ctx.delisting(instrument).is_some() && opens(ctx, instrument, *side) => {
                    metrics::counter!(m::REJECTED, "source" => "risk", "code" => "delisting")
                        .increment(1);
                    Err("instrument is scheduled for delisting: only orders that reduce the position".to_owned())
                }
                Signal::Submit { .. } | Signal::Reduce { .. } => {
                    let (instrument, side, size, kind, reduce) = match &signal {
                        Signal::Submit {
                            instrument,
                            side,
                            size,
                            kind,
                        } => (instrument, side, size, kind, false),
                        Signal::Reduce {
                            instrument,
                            side,
                            qty,
                            kind,
                        } => (instrument, side, &Size::Base(*qty), kind, true),
                        Signal::Cancel(_) => continue,
                    };
                    let reference = ctx.last_candle(instrument).map(|c| c.close);
                    let now = ctx.now().unwrap_or(Timestamp::from_millis(i64::MIN));
                    let market = risk::MarketPrices {
                        last_close: reference,
                        average: ctx
                            .instrument(instrument)
                            .and_then(|i| i.average_price(ctx.history(instrument), now)),
                    };
                    let id = self.mint_id();
                    match risk::to_order(
                        id,
                        ctx.instrument(instrument),
                        *side,
                        *size,
                        *kind,
                        market,
                        now,
                    )
                    .and_then(|o| {
                        if reduce {
                            o.with_reduce_only().map_err(risk::RiskRejection::Invalid)
                        } else {
                            Ok(o)
                        }
                    }) {
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
                                    tracing::info!(order = %self.broker.client_order_id(&id), %instrument, ?side, "order submitted");
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
            ..Default::default()
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

    /// Bids a limit at twice the last close.
    struct Overbid;
    impl Strategy for Overbid {
        fn name(&self) -> &str {
            "overbid"
        }
        fn on_candle(&mut self, _: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
            vec![Signal::Submit {
                instrument: c.instrument.clone(),
                side: bot_core::Side::Buy,
                size: bot_core::Size::Base(Qty::new(dec!(1))),
                kind: bot_core::OrderKind::Limit {
                    price: Price::new(c.close.get() * dec!(2)),
                    tif: bot_core::TimeInForce::Gtc,
                },
            }]
        }
    }

    /// With BTCUSDT's band (buys up to 1.2x the average) a limit at 2x the
    /// close is refused before the broker, each bar, as Binance would with
    /// -1013. The test candles' average is their close (quote volume =
    /// close, volume 1). Without a band the same bids are accepted.
    #[tokio::test]
    async fn limit_beyond_the_price_band_is_refused_by_risk() {
        let mut provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        provider.instrument = bot_core::Instrument::new(bot_core::InstrumentSpec {
            max_open_orders: None,
            max_open_stops: None,
            price_band: Some(bot_core::PriceBand {
                bid_up: dec!(1.2),
                bid_down: dec!(0.5),
                ask_up: dec!(2),
                ask_down: dec!(0.8),
                avg_minutes: 5,
            }),
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let r = run(&provider, &mut Overbid, Params::default(), &config())
            .await
            .unwrap();
        assert_eq!(r.rejections.len(), 3);
        assert_eq!(
            r.rejections[0].reason,
            "price outside the venue's band around its average price"
        );
        let plain = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let r = run(&plain, &mut Overbid, Params::default(), &config())
            .await
            .unwrap();
        assert!(r.rejections.is_empty(), "{:?}", r.rejections);
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
        // The one fill (the buy decided at c0, filled at c1's open) is
        // carried by c1's snapshot alone: an observer recording
        // `new_fills` sees each fill once.
        let carried: Vec<usize> = keep.0.iter().map(|s| s.new_fills.len()).collect();
        assert_eq!(carried, [0, 1, 0]);
        assert_eq!(keep.0[1].new_fills, r.fills);
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

    /// A strategy that reports a fault stops the run, and nothing it
    /// queued in the failing call reaches the broker: c0's buy is placed,
    /// c1's buy (queued in the same call that failed) never is.
    #[tokio::test]
    async fn a_strategy_fault_stops_the_run_before_routing() {
        #[derive(Default)]
        struct Breaks {
            bars: usize,
            placed: Vec<String>,
            fault: Option<String>,
        }
        impl Strategy for Breaks {
            fn name(&self) -> &str {
                "breaks"
            }
            fn on_candle(&mut self, _: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
                self.bars += 1;
                if self.bars == 2 {
                    self.fault = Some("boom".into());
                }
                vec![Signal::Submit {
                    instrument: c.instrument.clone(),
                    side: bot_core::Side::Buy,
                    size: bot_core::Size::Base(Qty::new(dec!(1))),
                    kind: bot_core::OrderKind::Market,
                }]
            }
            fn on_order(&mut self, _: &mut Context, o: &Order) {
                self.placed.push(o.id.to_string());
            }
            fn fault(&self) -> Option<String> {
                self.fault.clone()
            }
        }
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let mut s = Breaks::default();
        let err = run(&provider, &mut s, Params::default(), &config())
            .await
            .unwrap_err();
        assert!(matches!(&err, EngineError::StrategyFault(r) if r == "boom"));
        assert_eq!(s.placed, vec!["o-1"]);
        assert_eq!(s.bars, 2, "no event after the fault");
    }

    /// A fault in `on_start` stops the run before its signals are routed.
    #[tokio::test]
    async fn a_fault_at_start_routes_nothing() {
        struct BadStart;
        impl Strategy for BadStart {
            fn name(&self) -> &str {
                "bad_start"
            }
            fn on_start(&mut self, ctx: &mut Context) {
                ctx.submit(Signal::Submit {
                    instrument: btcusdt().id.clone(),
                    side: bot_core::Side::Buy,
                    size: bot_core::Size::Base(Qty::new(dec!(1))),
                    kind: bot_core::OrderKind::Market,
                });
            }
            fn on_order(&mut self, _: &mut Context, _: &Order) {
                panic!("an order was routed from a failed strategy");
            }
            fn fault(&self) -> Option<String> {
                Some("bad script".into())
            }
        }
        let provider = TestProvider::new(three_candles(), dec!(0), dec!(0));
        let err = run(&provider, &mut BadStart, Params::default(), &config())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::StrategyFault(_)));
    }

    /// buy_and_hold puts 1,000 into 10 BTC at c1's open (100); closes
    /// then fall 100, 90, 80, 70, 60, 50. Kill switch: capital 1,000, 25%
    /// → floor 750 under the 1,000 peak. Equity 900 at c2, 800 at c3, 700
    /// at c4 → tripped: the engine sells the 10 (reduce-only) at c5's open
    /// (60: each bar opens at its own price) and, flat at c5, ends the
    /// run: c6 is never read. Without the
    /// switch the 10 are held to the end.
    #[tokio::test]
    async fn the_kill_switch_closes_everything_and_ends_the_run() {
        let bars = |closes: &[&str]| -> Vec<MarketEvent> {
            closes
                .iter()
                .enumerate()
                .map(|(i, p)| MarketEvent::Candle(candle(i as i64, [p, p, p, p])))
                .collect()
        };
        let events = bars(&["100", "100", "90", "80", "70", "60", "50"]);
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let config = EngineConfig {
            kill_switch: Some(KillSwitch {
                capital: Notional::new(dec!(1000)),
                max_drawdown: dec!(0.25),
            }),
            ..config()
        };
        let r = run(&provider, s.as_mut(), params.clone(), &config)
            .await
            .unwrap();
        let got: Vec<(i64, bot_core::Side, Qty, Decimal)> = r
            .fills
            .iter()
            .map(|f| (f.ts.as_millis() / HOUR, f.side, f.qty, f.price.get()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, bot_core::Side::Buy, Qty::new(dec!(10)), dec!(100)),
                (5, bot_core::Side::Sell, Qty::new(dec!(10)), dec!(60)),
            ]
        );
        assert!(r.positions[0].is_flat());
        assert_eq!(r.events, 6, "c6 never read");
        let why = r.killed.expect("tripped");
        assert!(
            why.contains("equity 700") && why.contains("peak 1000"),
            "{why}"
        );

        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &self::config())
            .await
            .unwrap();
        assert_eq!(r.fills.len(), 1);
        assert_eq!(r.killed, None);
        assert_eq!(r.events, 7);
    }

    /// The peak follows equity up: 1,000 → 10 BTC at 100, closes 100, 150
    /// (equity 1,500: the new peak), 120 (1,200 > 1,125 = 1,500 · 0.75),
    /// 110 (1,100 ≤ 1,125) → tripped at c4, sold at c5's open.
    #[tokio::test]
    async fn the_kill_switch_measures_from_the_highest_equity() {
        let events: Vec<MarketEvent> = ["100", "100", "150", "120", "110", "110", "110"]
            .iter()
            .enumerate()
            .map(|(i, p)| MarketEvent::Candle(candle(i as i64, [p, p, p, p])))
            .collect();
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let config = EngineConfig {
            kill_switch: Some(KillSwitch {
                capital: Notional::new(dec!(1000)),
                max_drawdown: dec!(0.25),
            }),
            ..config()
        };
        let r = run(&provider, s.as_mut(), params, &config).await.unwrap();
        let sells: Vec<i64> = r
            .fills
            .iter()
            .filter(|f| f.side == bot_core::Side::Sell)
            .map(|f| f.ts.as_millis() / HOUR)
            .collect();
        assert_eq!(sells, vec![5]);
        assert!(r.killed.unwrap().contains("peak 1500"));
    }

    fn delisting(at: Option<i64>, ts: i64) -> MarketEvent {
        MarketEvent::Delisting {
            instrument: btcusdt().id.clone(),
            at: at.map(Timestamp::from_millis),
            ts: Timestamp::from_millis(ts),
        }
    }

    fn flat_bars(from: i64, to: i64) -> Vec<MarketEvent> {
        (from..to)
            .map(|i| MarketEvent::Candle(candle(i, ["100", "100", "100", "100"])))
            .collect()
    }

    /// Announced before the first candle: buy_and_hold's buy is refused,
    /// not fatal, and says why.
    #[tokio::test]
    async fn no_new_buys_once_a_delisting_is_announced() {
        let mut events = vec![delisting(Some(100 * HOUR), 0)];
        events.extend(flat_bars(0, 3));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert!(r.fills.is_empty());
        assert_eq!(r.rejections.len(), 1);
        assert!(
            r.rejections[0].reason.contains("delisting"),
            "{:?}",
            r.rejections
        );

        // Withdrawn before the first candle: the buy goes through.
        let mut events = vec![delisting(Some(100 * HOUR), 0), delisting(None, 0)];
        events.extend(flat_bars(0, 3));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert_eq!(r.fills.len(), 1);
    }

    /// buy_and_hold bought 10 BTC at c1's open (no fees). Delisting
    /// announced after c1, at 5h, exit lead 2h: from 3h on the engine
    /// sells. c2 closes at 3h - 1 ms (too early); c3 at 4h - 1 ms sells 10,
    /// filled at c4's open, before the delisting. buy_and_hold never sells:
    /// the engine did.
    #[tokio::test]
    async fn the_engine_sells_before_the_delisting() {
        let mut events = flat_bars(0, 2);
        events.push(delisting(Some(5 * HOUR), 2 * HOUR - 1));
        events.extend(flat_bars(2, 6));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let config = EngineConfig {
            delist_exit: Duration::from_secs(2 * 3600),
            ..config()
        };
        let r = run(&provider, s.as_mut(), params, &config).await.unwrap();
        let got: Vec<(i64, bot_core::Side, Qty)> = r
            .fills
            .iter()
            .map(|f| (f.ts.as_millis() / HOUR, f.side, f.qty))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, bot_core::Side::Buy, Qty::new(dec!(10))),
                (4, bot_core::Side::Sell, Qty::new(dec!(10))),
            ]
        );
        assert!(r.positions[0].is_flat());
        assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    }

    /// Announced too late to sell in time: lead 1h, delisting at 5h, the
    /// exit's one chance is c4 (closes 5h - 1 ms); its sell would fill at
    /// c5's open, but by then the venue has delisted the coin and cancels
    /// it. After 5h the engine sends nothing more (no refusal each event):
    /// the holding is stuck, as it would be live.
    #[tokio::test]
    async fn after_the_delisting_the_engine_stops_trying() {
        let mut events = flat_bars(0, 4);
        events.push(delisting(Some(5 * HOUR), 4 * HOUR - 1));
        events.extend(flat_bars(4, 9));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let config = EngineConfig {
            delist_exit: Duration::from_secs(3600),
            ..config()
        };
        let r = run(&provider, s.as_mut(), params, &config).await.unwrap();
        assert_eq!(r.fills.len(), 1, "only the buy");
        assert_eq!(r.positions[0].qty(), Qty::new(dec!(10)));
        assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    }

    /// Halted before the first candle: buy_and_hold's buy is refused
    /// (not fatal); once trading resumes a later strategy's buy goes
    /// through.
    #[tokio::test]
    async fn no_orders_while_halted() {
        let halt = |trading: bool, ts: i64| MarketEvent::TradingStatus {
            instrument: btcusdt().id.clone(),
            trading,
            ts: Timestamp::from_millis(ts),
        };
        let mut events = vec![halt(false, 0)];
        events.extend(flat_bars(0, 3));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert!(r.fills.is_empty());
        assert!(
            r.rejections[0].reason.contains("halted"),
            "{:?}",
            r.rejections
        );

        let mut events = vec![halt(false, 0), halt(true, 0)];
        events.extend(flat_bars(0, 3));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert_eq!(r.fills.len(), 1);
    }

    /// A market closed for the session isn't a halt: buy_and_hold's buy
    /// goes out while closed and fills (the venue fills it at the open, as
    /// the paper broker fills at the next bar's open), no rejection.
    #[tokio::test]
    async fn orders_go_out_while_the_market_is_closed() {
        let mut events = vec![MarketEvent::MarketSession {
            instrument: btcusdt().id.clone(),
            open: false,
            ts: Timestamp::from_millis(0),
        }];
        events.extend(flat_bars(0, 3));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let (mut s, params) = buy_and_hold("1000");
        let r = run(&provider, s.as_mut(), params, &config()).await.unwrap();
        assert!(r.rejections.is_empty(), "{:?}", r.rejections);
        assert_eq!(r.fills.len(), 1);
    }

    /// A resting limit buy (far below the market, never fills) on an
    /// instrument entering its exit window is cancelled by the engine; with
    /// nothing held there is nothing to sell.
    #[tokio::test]
    async fn the_exit_cancels_working_orders_first() {
        struct Rests(bool);
        impl Strategy for Rests {
            fn name(&self) -> &str {
                "rests"
            }
            fn on_candle(&mut self, _: &mut Context, c: &bot_core::Candle) -> Vec<Signal> {
                if std::mem::replace(&mut self.0, true) {
                    return Vec::new();
                }
                vec![Signal::Submit {
                    instrument: c.instrument.clone(),
                    side: bot_core::Side::Buy,
                    size: bot_core::Size::Base(Qty::new(dec!(1))),
                    kind: bot_core::OrderKind::Limit {
                        price: Price::new(dec!(50)),
                        tif: bot_core::TimeInForce::Gtc,
                    },
                }]
            }
        }
        struct LastOpen(usize);
        impl Observer for LastOpen {
            fn on_snapshot(&mut self, s: &Snapshot) {
                self.0 = s.open_orders.len();
            }
        }
        let mut events = flat_bars(0, 2);
        events.push(delisting(Some(4 * HOUR), 2 * HOUR - 1));
        events.extend(flat_bars(2, 4));
        let provider = TestProvider::new(events, dec!(0), dec!(0));
        let config = EngineConfig {
            delist_exit: Duration::from_secs(2 * 3600),
            ..config()
        };
        let mut seen = LastOpen(99);
        run_observed(
            &provider,
            &mut Rests(false),
            Params::default(),
            &config,
            &mut seen,
            None,
        )
        .await
        .unwrap();
        assert_eq!(seen.0, 0, "the resting buy was cancelled");
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
