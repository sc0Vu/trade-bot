//! `grid`: a spot grid on one coin.
//!
//! `grids + 1` price levels `L0 < L1 < … < Ln` split `[lower, upper]`
//! (equal steps, or equal ratios). Each gap `i` buys `quote_per_grid` worth
//! at `Li` with a GTC limit and, once bought, sells that lot one level up at
//! `L(i+1)`. A round trip earns one step less two fees, so the build refuses
//! a grid whose narrowest step doesn't clear them. On the first evaluation
//! every gap gets its buy: those above the price are marketable and fill at
//! the next open, the usual "neutral" opening that holds a lot for every
//! level above the price.
//!
//! Venue price band (Binance `PERCENT_PRICE_BY_SIDE`, a buy at most 1.2×
//! the average price on BTCUSDT): a buy above the band's top is placed at
//! the top instead, and a sell below its bottom at the bottom. Both are
//! marketable either way and fill at the market, so the opening is the
//! same, without the venue's `-1013`. A buy below the bottom or a sell
//! above the top would be refused outright: its gap waits, unplaced, until
//! the price comes near.
//!
//! What it bets on: a price that oscillates inside the range. It loses to
//! a trend either way: falling, it buys every level and ends holding all
//! of them below cost (`stop_price`, if set, sells everything at market and
//! stops once the close goes below it); rising, it sells out and sits in
//! cash below the price. Expected edge is unverified.
//!
//! Orders: every working order belongs to one gap, known by the id
//! `Strategy::on_order` reports, so a fill is credited to its gap even when
//! the order fills before it ever shows in `open_orders`. A venue may fill
//! an order in many parts: fills only add up, and a gap moves on once its
//! order is neither open nor being cancelled (no fill can still arrive). A
//! refused order puts its gap back and pauses new orders until the next
//! fill, so a grid out of cash doesn't resubmit every bar.
//!
//! Restart: a live restart's reconcile cancels working orders, and fills
//! made while down are already in the books' position. So saved gaps are
//! settled (what a working order got is kept) and squared with the
//! position: extra coin went to the buys that were working, nearest the
//! price first; missing coin was sold by the sells that were working,
//! lowest first. Fills reported for orders of the earlier run are ignored
//! (already in the position). State that can't be squared is refused, and
//! a fresh start gives the coin held to the gaps above the price.
//!
//! Decisions use only closed candles and positions read through `Context`
//! (invariants 2 and 5); money is `Decimal` (invariant 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, Fee, Fill, Instrument, InstrumentId, Notional, Order, OrderId, OrderKind,
    Price, Qty, Side, Signal, Size, TimeInForce,
};
use rust_decimal::MathematicalOps;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

const NAME: &str = "grid";

/// How the levels split the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Spacing {
    /// Equal price steps: the profit per step, as a fraction, shrinks
    /// toward the top.
    Arithmetic,
    /// Equal ratios: the same fraction per step everywhere; suits a wide
    /// range.
    Geometric,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GridParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Lowest level (`"50000"`).
    lower: Price,
    /// Highest level (`"70000"`).
    upper: Price,
    /// Gaps between levels (`10` = 11 levels). At least 2.
    grids: usize,
    /// `"arithmetic"` or `"geometric"`. Required: they put the levels in
    /// different places.
    spacing: Spacing,
    /// Quote each buy spends (`"100"`). The grid can hold up to `grids`
    /// lots, so it may need `grids * quote_per_grid` in all; buys beyond
    /// the balance are refused like Binance's `-2010`.
    quote_per_grid: Notional,
    /// Fee per fill assumed (`"0.001"`): a step must beat two of them.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage on the market sell `stop_price` triggers, in basis points
    /// (`"5"`). Required and unused in decisions, as in the other
    /// built-ins; match the backtest's `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
    /// Optional, below `lower`: once a close is below it, cancel every
    /// order, sell everything at market, and stop.
    stop_price: Option<Price>,
}

/// One gap `i`: buys at `levels[i]`, sells at `levels[i + 1]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Gap {
    /// Wants its buy.
    Empty,
    /// Buy working; `got` = base received so far, net of a base fee.
    Buying { got: Qty },
    /// Holds a lot; wants its sell.
    Held { qty: Qty },
    /// Sell of `qty` (a lot multiple) working; `sold` so far.
    Selling { qty: Qty, sold: Qty },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Running,
    /// Below `stop_price`: orders cancelled, selling everything once
    /// nothing can still fill.
    Stopping,
    /// Not trading; the reason.
    Stopped(String),
}

/// What `save_state` writes.
#[derive(Serialize, Deserialize)]
struct Saved {
    v: u64,
    stage: Stage,
    /// Set once the first evaluation has placed the grid.
    started: bool,
    gaps: Vec<Gap>,
    /// Base held but too little to sell (lot remainders of base fees).
    dust: Qty,
}

/// See the module docs.
pub struct Grid {
    p: GridParams,
    /// Tick-rounded, set by `on_start`.
    levels: Vec<Price>,
    gaps: Vec<Gap>,
    dust: Qty,
    stage: Stage,
    started: bool,
    /// After a refusal: no new orders until the next fill.
    blocked: bool,
    /// Gap of each working order. Not saved: a restart's orders are new.
    working: HashMap<usize, OrderId>,
    /// Gaps of submits routed but not yet reported (`None`: the stop's
    /// market sell), in signal order, as `on_order` / `on_rejected` come.
    routing: VecDeque<Option<usize>>,
}

/// The raw levels, `lower` to `upper` inclusive.
fn raw_levels(p: &GridParams) -> Vec<Decimal> {
    let (lo, hi, n) = (p.lower.get(), p.upper.get(), p.grids);
    let mut v = Vec::with_capacity(n + 1);
    match p.spacing {
        Spacing::Arithmetic => {
            let step = (hi - lo) / Decimal::from(n);
            v.extend((0..n).map(|i| lo + step * Decimal::from(i)));
        }
        Spacing::Geometric => {
            let r = (hi / lo).powd(Decimal::ONE / Decimal::from(n));
            let mut l = lo;
            for _ in 0..n {
                v.push(l);
                l *= r;
            }
        }
    }
    v.push(hi);
    v
}

/// Every round trip (buy at one level, sell at the next, a fee on each)
/// ends with more quote than it spent.
fn steps_clear_fees(levels: &[Decimal], fee: Decimal) -> bool {
    let keep = (Decimal::ONE - fee) * (Decimal::ONE - fee);
    levels.windows(2).all(|w| w[1] * keep > w[0])
}

impl Grid {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: GridParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if !p.lower.is_positive() || p.upper <= p.lower {
            return bad("need 0 < lower < upper");
        }
        if p.grids < 2 {
            return bad("grids must be >= 2");
        }
        if !p.quote_per_grid.is_positive() {
            return bad("quote_per_grid must be positive");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        if p.stop_price
            .is_some_and(|s| !s.is_positive() || s >= p.lower)
        {
            return bad("stop_price must be in (0, lower)");
        }
        if !steps_clear_fees(&raw_levels(&p), p.fee) {
            return bad("a grid step doesn't cover two fees: every round trip would lose");
        }
        let gaps = vec![Gap::Empty; p.grids];
        Ok(Box::new(Self {
            p,
            levels: Vec::new(),
            gaps,
            dust: Qty::ZERO,
            stage: Stage::Running,
            started: false,
            blocked: false,
            working: HashMap::new(),
            routing: VecDeque::new(),
        }))
    }

    fn stop(&mut self, reason: &str) {
        self.stage = Stage::Stopped(reason.to_owned());
    }

    /// Base a buy at gap `i` is expected to bring: its lot at the level,
    /// less the fee. Caps what a restart credits to the gap.
    fn nominal(&self, inst: &Instrument, i: usize) -> Qty {
        let q = inst
            .qty_for(self.p.quote_per_grid, self.levels[i])
            .map(|q| inst.round_qty(q))
            .unwrap_or(Qty::ZERO);
        q * (Decimal::ONE - self.p.fee)
    }

    /// No order of ours on the coin can still fill.
    fn idle(&self, ctx: &Context) -> bool {
        ctx.open_orders(&self.p.coin).next().is_none()
            && ctx.cancelling_orders(&self.p.coin).next().is_none()
    }

    /// Settles gaps whose order is done: neither open nor being cancelled,
    /// so every fill of it has arrived.
    fn progress(&mut self, ctx: &Context, inst: &Instrument) {
        let live = |id: &OrderId| {
            ctx.open_orders(&self.p.coin).any(|o| o.id == *id)
                || ctx.cancelling_orders(&self.p.coin).any(|o| o.id == *id)
        };
        let done: Vec<usize> = self
            .working
            .iter()
            .filter(|(_, id)| !live(id))
            .map(|(i, _)| *i)
            .collect();
        for i in done {
            self.working.remove(&i);
            self.gaps[i] = match self.gaps[i].clone() {
                Gap::Buying { got } if got.is_positive() => Gap::Held { qty: got },
                Gap::Selling { qty, sold } if sold < qty => {
                    let rest = qty - sold;
                    if inst.check_order(rest, self.levels[i + 1]).is_ok() {
                        Gap::Held { qty: rest }
                    } else {
                        self.dust += rest;
                        Gap::Empty
                    }
                }
                Gap::Buying { .. } | Gap::Selling { .. } => Gap::Empty,
                other => other,
            };
        }
    }

    /// A fresh start holding coin (no usable state): the coin goes to the
    /// gaps above the price, nearest first, a lot each as if the neutral
    /// opening had bought it; a remainder too small to sell at its level
    /// is dust, and the gaps left get their buys.
    fn adopt(&mut self, inst: &Instrument, held: Qty, close: Price) {
        let mut left = held;
        for i in 0..self.gaps.len() {
            if self.levels[i + 1] <= close {
                continue;
            }
            let q = self.nominal(inst, i).min(left);
            if inst
                .check_order(inst.round_qty(q), self.levels[i + 1])
                .is_err()
            {
                break;
            }
            self.gaps[i] = Gap::Held { qty: q };
            left -= q;
        }
        self.dust += left;
    }

    fn limit(&self, side: Side, size: Size, price: Price) -> Signal {
        Signal::Submit {
            instrument: self.p.coin.clone(),
            side,
            size,
            kind: OrderKind::Limit {
                price,
                tif: TimeInForce::Gtc,
            },
        }
    }

    /// The price to place a gap's order at, given the venue's price band
    /// around `avg`: its `level`, or the band's edge for a level past the
    /// marketable edge (a buy above the top, a sell below the bottom: it
    /// fills at the market either way). `None` for a level past the passive
    /// edge, which the venue would refuse. No band or no average: `level`.
    fn band_price(
        inst: &Instrument,
        avg: Option<Price>,
        side: Side,
        level: Price,
    ) -> Option<Price> {
        let Some((lo, hi)) = avg.and_then(|a| inst.price_range(side, a)) else {
            return Some(level);
        };
        match side {
            Side::Buy if level > hi => Some(inst.round_price(hi, Side::Buy)),
            Side::Sell if level < lo => Some(inst.round_price(lo, Side::Sell)),
            Side::Buy if level < lo => None,
            Side::Sell if level > hi => None,
            _ => Some(level),
        }
    }

    fn evaluate(&mut self, ctx: &Context, inst: &Instrument, close: Price) -> Vec<Signal> {
        match self.stage {
            Stage::Stopped(_) => return Vec::new(),
            Stage::Stopping => {
                if !self.idle(ctx) {
                    return Vec::new();
                }
                let units = inst.round_qty(ctx.position(&self.p.coin).qty());
                self.stop("stop_price reached");
                if inst.check_market(units, close, Side::Sell).is_err() {
                    return Vec::new();
                }
                self.routing.push_back(None);
                return vec![Signal::Submit {
                    instrument: self.p.coin.clone(),
                    side: Side::Sell,
                    size: Size::Base(units),
                    kind: OrderKind::Market,
                }];
            }
            Stage::Running => {}
        }
        if self.p.stop_price.is_some_and(|s| close < s) {
            self.stage = Stage::Stopping;
            let cancels: Vec<Signal> = ctx
                .open_orders(&self.p.coin)
                .map(|o| Signal::Cancel(o.id.clone()))
                .collect();
            return cancels;
        }
        if !self.started {
            self.started = true;
            let held = ctx.position(&self.p.coin).qty();
            if held.is_positive() {
                self.adopt(inst, held, close);
            }
        }
        if self.blocked {
            return Vec::new();
        }
        let avg = ctx
            .now()
            .and_then(|now| inst.average_price(ctx.history(&self.p.coin), now));
        let mut out = Vec::new();
        for i in 0..self.gaps.len() {
            match self.gaps[i].clone() {
                Gap::Empty => {
                    let Some(price) = Self::band_price(inst, avg, Side::Buy, self.levels[i]) else {
                        continue;
                    };
                    out.push(self.limit(Side::Buy, Size::Quote(self.p.quote_per_grid), price));
                    self.gaps[i] = Gap::Buying { got: Qty::ZERO };
                    self.routing.push_back(Some(i));
                }
                Gap::Held { qty } => {
                    let Some(price) = Self::band_price(inst, avg, Side::Sell, self.levels[i + 1])
                    else {
                        continue;
                    };
                    let lot = inst.round_qty(qty);
                    if inst.check_order(lot, price).is_err() {
                        self.dust += qty;
                        self.gaps[i] = Gap::Empty;
                        continue;
                    }
                    self.dust += qty - lot;
                    out.push(self.limit(Side::Sell, Size::Base(lot), price));
                    self.gaps[i] = Gap::Selling {
                        qty: lot,
                        sold: Qty::ZERO,
                    };
                    self.routing.push_back(Some(i));
                }
                Gap::Buying { .. } | Gap::Selling { .. } => {}
            }
        }
        out
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Grid::build }
}

impl Strategy for Grid {
    fn name(&self) -> &str {
        NAME
    }

    /// Stage, gaps and dust: which levels hold a lot.
    fn save_state(&self) -> Option<serde_json::Value> {
        serde_json::to_value(Saved {
            v: super::STATE_VERSION,
            stage: self.stage.clone(),
            started: self.started,
            gaps: self.gaps.clone(),
            dust: self.dust,
        })
        .ok()
    }

    /// Squares the saved gaps with the books (see the module docs). A
    /// stopped grid starts fresh: a restart is the operator stepping in,
    /// and below `stop_price` it stops again before buying anything.
    fn restore_state(
        &mut self,
        state: serde_json::Value,
        ctx: &Context,
    ) -> Result<(), StrategyError> {
        let err = |reason: &str| Err(super::state_error(NAME, reason));
        if let Stage::Stopped(_) = self.stage {
            return err("universe doesn't fit; staying stopped");
        }
        super::check_version(NAME, &state)?;
        let saved: Saved =
            serde_json::from_value(state).map_err(|e| super::state_error(NAME, e))?;
        let Some(inst) = ctx.instrument(&self.p.coin) else {
            return err("coin not in the universe");
        };
        if saved.gaps.len() != self.gaps.len() {
            return err("saved for another number of grids");
        }
        if let Stage::Stopped(_) = saved.stage {
            return err("was stopped; starting fresh");
        }
        // The reconcile closed every working order: keep what each got.
        let mut buying = Vec::new();
        let mut selling = Vec::new();
        let mut gaps: Vec<Gap> = saved
            .gaps
            .into_iter()
            .enumerate()
            .map(|(i, g)| match g {
                Gap::Buying { got } => {
                    buying.push(i);
                    if got.is_positive() {
                        Gap::Held { qty: got }
                    } else {
                        Gap::Empty
                    }
                }
                Gap::Selling { qty, sold } => {
                    selling.push(i);
                    if sold < qty {
                        Gap::Held { qty: qty - sold }
                    } else {
                        Gap::Empty
                    }
                }
                other => other,
            })
            .collect();
        let accounted: Qty = gaps
            .iter()
            .map(|g| match g {
                Gap::Held { qty } => *qty,
                _ => Qty::ZERO,
            })
            .sum::<Qty>()
            + saved.dust;
        let held = ctx.position(&self.p.coin).qty();
        let mut dust = saved.dust;
        if held > accounted {
            // Bought while down: by the buys that were working, nearest the
            // price (highest) first.
            let mut extra = held - accounted;
            for &i in buying.iter().rev() {
                let have = match gaps[i] {
                    Gap::Held { qty } => qty,
                    _ => Qty::ZERO,
                };
                let add = (self.nominal(inst, i) - have).max(Qty::ZERO).min(extra);
                if add.is_positive() {
                    gaps[i] = Gap::Held { qty: have + add };
                    extra -= add;
                }
            }
            dust += extra;
        } else if held < accounted {
            // Sold while down: by the sells that were working, lowest first.
            let mut missing = accounted - held;
            for &i in &selling {
                if let Gap::Held { qty } = gaps[i] {
                    let take = qty.min(missing);
                    missing -= take;
                    gaps[i] = if take == qty {
                        Gap::Empty
                    } else {
                        Gap::Held { qty: qty - take }
                    };
                }
            }
            let from_dust = missing.min(dust);
            dust -= from_dust;
            missing -= from_dust;
            if missing.is_positive() {
                return err("the books hold less than the saved gaps");
            }
        }
        self.gaps = gaps;
        self.dust = dust;
        self.stage = saved.stage;
        self.started = saved.started;
        self.blocked = false;
        self.working.clear();
        self.routing.clear();
        Ok(())
    }

    /// Computes the levels on the coin's tick, and checks every gap's buy
    /// and the fees after rounding, and that the venue lets every gap keep
    /// its order open. A grid that can't trade stops.
    fn on_start(&mut self, ctx: &mut Context) {
        let Some(inst) = ctx.instrument(&self.p.coin) else {
            return self.stop("coin not in the universe");
        };
        // One working order per gap at most; past the cap the venue refuses
        // the rest and those gaps never trade.
        if inst
            .max_open_orders
            .is_some_and(|cap| self.p.grids > cap as usize)
        {
            return self.stop("more grids than the venue allows open orders");
        }
        // 12 dp first: `powd` is approximate, and a level a hair under a
        // tick (199.999…) must not floor a whole tick down.
        let levels: Vec<Price> = raw_levels(&self.p)
            .into_iter()
            .map(|l| inst.round_price(Price::new(l.round_dp(12)), Side::Buy))
            .collect();
        if levels.windows(2).any(|w| w[1] <= w[0]) {
            return self.stop("levels collapse on the tick: fewer grids or a wider range");
        }
        let raw: Vec<Decimal> = levels.iter().map(|l| l.get()).collect();
        if !steps_clear_fees(&raw, self.p.fee) {
            return self.stop("on the tick, a step doesn't cover two fees");
        }
        for &l in &levels[..levels.len() - 1] {
            let q = inst
                .qty_for(self.p.quote_per_grid, l)
                .map(|q| inst.round_qty(q));
            if q.map_or(true, |q| inst.check_order(q, l).is_err()) {
                return self.stop("quote_per_grid is below the venue's minimum at a level");
            }
        }
        self.levels = levels;
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        if c.instrument != self.p.coin || matches!(self.stage, Stage::Stopped(_)) {
            return Vec::new();
        }
        let Some(inst) = ctx.instrument(&self.p.coin).cloned() else {
            return Vec::new();
        };
        self.progress(ctx, &inst);
        self.evaluate(ctx, &inst, c.close)
    }

    /// Credits a fill to its gap; one of an earlier run's orders (reported
    /// at restart) is already in the position the restore squared with.
    fn on_fill(&mut self, _ctx: &mut Context, f: &Fill) {
        self.blocked = false;
        let Some(i) = self
            .working
            .iter()
            .find(|(_, id)| **id == f.order_id)
            .map(|(i, _)| *i)
        else {
            return;
        };
        match &mut self.gaps[i] {
            Gap::Buying { got } => {
                let fee = match f.fee {
                    Fee::Base(q) => q,
                    Fee::Quote(_) | Fee::Other { .. } => Qty::ZERO,
                };
                *got += f.qty - fee;
            }
            Gap::Selling { sold, .. } => *sold += f.qty,
            Gap::Empty | Gap::Held { .. } => {}
        }
    }

    fn on_order(&mut self, _ctx: &mut Context, order: &Order) {
        if let Some(Some(i)) = self.routing.pop_front() {
            self.working.insert(i, order.id.clone());
        }
    }

    /// A refused submit puts its gap back and pauses new orders until the
    /// next fill. A refused stop sell stops the grid: the operator must
    /// look.
    fn on_rejected(&mut self, _ctx: &mut Context, signal: &Signal, reason: &str) {
        if !matches!(signal, Signal::Submit { .. }) {
            return;
        }
        self.blocked = true;
        match self.routing.pop_front() {
            Some(Some(i)) => {
                self.gaps[i] = match self.gaps[i].clone() {
                    Gap::Selling { qty, .. } => Gap::Held { qty },
                    _ => Gap::Empty,
                };
            }
            Some(None) => self.stop(&format!("stop sell refused: {reason}")),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{Liquidity, OrderSpec, Position, Timestamp};
    use rust_decimal_macros::dec;

    /// 90..110 in 4 equal gaps: levels 90, 95, 100, 105, 110.
    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               lower = "90"
               upper = "110"
               grids = 4
               spacing = "arithmetic"
               quote_per_grid = "100"
               fee = "0.001"
               slippage_bps = "5"
               {extra}"#
        ))
        .unwrap()
    }

    fn start(p: Params) -> (Box<dyn Strategy>, Context) {
        let mut ctx = driver::new(vec![btcusdt()], p.clone(), 10);
        let mut s = build(NAME, &p).unwrap();
        s.on_start(&mut ctx);
        (s, ctx)
    }

    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    fn px(p: &str) -> Price {
        p.parse().unwrap()
    }

    fn buy(level: &str) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(100))),
            kind: OrderKind::Limit {
                price: px(level),
                tif: TimeInForce::Gtc,
            },
        }
    }

    fn sell(level: &str, qty: Decimal) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Limit {
                price: px(level),
                tif: TimeInForce::Gtc,
            },
        }
    }

    /// The order risk makes of `sig` (a quote limit becomes its lot at the
    /// limit), as `id`.
    fn order(sig: &Signal, id: &str) -> Order {
        let Signal::Submit {
            side, size, kind, ..
        } = sig
        else {
            panic!("{sig:?}");
        };
        let i = btcusdt();
        let size = match (size, kind) {
            (Size::Quote(n), OrderKind::Limit { price, .. }) => {
                Size::Base(i.round_qty(i.qty_for(*n, *price).unwrap()))
            }
            (s, _) => *s,
        };
        Order::new(OrderSpec {
            id: OrderId::new(id).unwrap(),
            instrument: i.id.clone(),
            side: *side,
            size,
            kind: *kind,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    /// Routes `sigs` as the engine does, all accepted: ids `o-{first}`…
    fn accept(s: &mut dyn Strategy, ctx: &mut Context, sigs: &[Signal], first: u32) -> Vec<Order> {
        sigs.iter()
            .zip(first..)
            .map(|(sig, n)| {
                let o = order(sig, &format!("o-{n}"));
                s.on_order(ctx, &o);
                o
            })
            .collect()
    }

    fn fill(id: &str, side: Side, qty: Decimal, price: &str) -> Fill {
        let fee = match side {
            Side::Buy => Fee::Base(Qty::new(qty * dec!(0.001))),
            Side::Sell => Fee::Quote(Notional::new(qty * px(price).get() * dec!(0.001))),
        };
        Fill::new(bot_core::FillSpec {
            order_id: OrderId::new(id).unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            qty: Qty::new(qty),
            price: px(price),
            fee,
            liquidity: Liquidity::Maker,
            ts: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn hold(ctx: &mut Context, qty: Decimal) {
        driver::set_position(
            ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(qty),
                Notional::new(qty * dec!(100)),
                Notional::ZERO,
            )
            .unwrap(),
        );
    }

    fn dust(s: &dyn Strategy) -> Decimal {
        s.save_state().unwrap()["dust"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn gaps(s: &dyn Strategy) -> serde_json::Value {
        s.save_state().unwrap()["gaps"].clone()
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
    }

    /// 100..101 in 10 gaps: a 0.1 step is 0.1% at most, under two 0.1%
    /// fees; the rest are plain range errors.
    #[test]
    fn rejects_bad_params() {
        for extra in [r#"stop_price = "95""#, r#"stop_price = "0""#] {
            assert!(build(NAME, &params(extra)).is_err(), "{extra}");
        }
        let with = |lower: &str, upper: &str, grids: u32, spacing: &str| {
            Params::from_toml(&format!(
                r#"coin = "binance:BTCUSDT"
                   lower = "{lower}"
                   upper = "{upper}"
                   grids = {grids}
                   spacing = "{spacing}"
                   quote_per_grid = "100"
                   fee = "0.001"
                   slippage_bps = "5""#
            ))
            .unwrap()
        };
        assert!(build(NAME, &with("100", "101", 10, "arithmetic")).is_err());
        assert!(build(NAME, &with("100", "101", 10, "geometric")).is_err());
        assert!(build(NAME, &with("110", "90", 4, "arithmetic")).is_err());
        assert!(build(NAME, &with("90", "110", 1, "arithmetic")).is_err());
        assert!(build(NAME, &with("90", "110", 4, "linear")).is_err());
        assert!(build(NAME, &with("90", "110", 4, "geometric")).is_ok());
    }

    /// First close 100: every gap gets its buy, those at 100 and 105
    /// marketable (the neutral opening).
    #[test]
    fn opens_with_a_buy_at_every_level() {
        let (mut s, mut ctx) = start(params(""));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "100"),
            vec![buy("90"), buy("95"), buy("100"), buy("105")]
        );
    }

    /// 40..160 in 4 gaps (levels 40, 70, 100, 130, 160) on BTCUSDT with
    /// Binance's band: buys within 0.5..=1.2x the average, sells within
    /// 0.8..=2x. A test candle's average is its close.
    fn start_banded() -> (Box<dyn Strategy>, Context) {
        let p = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               lower = "40"
               upper = "160"
               grids = 4
               spacing = "arithmetic"
               quote_per_grid = "100"
               fee = "0.001"
               slippage_bps = "5""#,
        )
        .unwrap();
        let inst = Instrument::new(bot_core::InstrumentSpec {
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
        let mut ctx = driver::new(vec![inst], p.clone(), 10);
        let mut s = build(NAME, &p).unwrap();
        s.on_start(&mut ctx);
        (s, ctx)
    }

    #[test]
    fn more_gaps_than_the_open_order_cap_stops() {
        let started = |cap: u32| {
            let p = params("");
            let inst = Instrument::new(bot_core::InstrumentSpec {
                max_open_orders: Some(cap),
                ..(*btcusdt()).clone()
            })
            .unwrap();
            let mut ctx = driver::new(vec![inst], p.clone(), 10);
            let mut s = build(NAME, &p).unwrap();
            s.on_start(&mut ctx);
            (s, ctx)
        };
        // 4 gaps: a cap of 3 leaves one gap that could never place its order.
        let (mut s, mut ctx) = started(3);
        assert!(bar(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert!(
            s.save_state().unwrap()["stage"]
                .to_string()
                .contains("more grids than the venue allows open orders"),
            "{:?}",
            s.save_state()
        );
        let (mut s, mut ctx) = started(4);
        assert_eq!(bar(s.as_mut(), &mut ctx, 0, "100").len(), 4);
    }

    /// Opening at 100 (buys allowed 50..=120): 40 waits, 70 and 100 go as
    /// usual, 130 goes at the band's top, 120 (still marketable). At 60
    /// (30..=72) the 40 buy can go.
    #[test]
    fn buys_stay_inside_the_venue_price_band() {
        let (mut s, mut ctx) = start_banded();
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "100"),
            vec![buy("70"), buy("100"), buy("120")]
        );
        assert_eq!(bar(s.as_mut(), &mut ctx, 1, "60"), vec![buy("40")]);
    }

    /// The 70 buy fills (100/70 → 1.42857 BTC, 1.42714143 net, lot
    /// 1.42714). At 130 (sells allowed 104..=260) its 100 sell goes at the
    /// band's bottom, 104 (still marketable). The 120 buy fills (100/120 →
    /// 0.83333, 0.83249667 net, lot 0.83249); its sell at 160 is above
    /// the band at 70 (56..=140), so it waits, and goes at 90 (72..=180).
    #[test]
    fn sells_stay_inside_the_venue_price_band() {
        let (mut s, mut ctx) = start_banded();
        let opening = bar(s.as_mut(), &mut ctx, 0, "100");
        let o = accept(s.as_mut(), &mut ctx, &opening, 1);
        driver::set_open_orders(&mut ctx, vec![o[1].clone(), o[2].clone()]);
        s.on_fill(&mut ctx, &fill("o-1", Side::Buy, dec!(1.42857), "70"));
        let up = bar(s.as_mut(), &mut ctx, 1, "130");
        assert_eq!(up, vec![sell("104", dec!(1.42714))]);

        let o4 = accept(s.as_mut(), &mut ctx, &up, 4);
        driver::set_open_orders(&mut ctx, vec![o[1].clone(), o4[0].clone()]);
        s.on_fill(&mut ctx, &fill("o-3", Side::Buy, dec!(0.83333), "120"));
        // At 70 the 40 buy is inside (35..=84) and goes; the 160 sell waits.
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "70"), vec![buy("40")]);
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 3, "90"),
            vec![sell("160", dec!(0.83249))]
        );
    }

    /// 100..400 in 2 equal ratios: r = 2, levels 100, 200, 400.
    #[test]
    fn geometric_levels_are_equal_ratios() {
        let p = Params::from_toml(
            r#"coin = "binance:BTCUSDT"
               lower = "100"
               upper = "400"
               grids = 2
               spacing = "geometric"
               quote_per_grid = "100"
               fee = "0.001"
               slippage_bps = "5""#,
        )
        .unwrap();
        let (mut s, mut ctx) = start(p);
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "300"),
            vec![buy("100"), buy("200")]
        );
    }

    /// The 95 buy fills (1.05263 BTC, fee 0.00105263 BTC): 1.05157737 held,
    /// sold one level up as lot 1.05157 (0.00000737 is dust). When that
    /// sell fills, the gap buys at 95 again.
    #[test]
    fn a_filled_buy_sells_one_level_up_and_back() {
        let (mut s, mut ctx) = start(params(""));
        let opening = bar(s.as_mut(), &mut ctx, 0, "100");
        let o = accept(s.as_mut(), &mut ctx, &opening, 1);
        driver::set_open_orders(&mut ctx, vec![o[0].clone(), o[2].clone(), o[3].clone()]);
        s.on_fill(&mut ctx, &fill("o-2", Side::Buy, dec!(1.05263), "95"));
        let up = bar(s.as_mut(), &mut ctx, 1, "96");
        assert_eq!(up, vec![sell("100", dec!(1.05157))]);
        assert_eq!(dust(s.as_ref()), dec!(0.00000737));

        let o5 = accept(s.as_mut(), &mut ctx, &up, 5);
        driver::set_open_orders(&mut ctx, vec![o[0].clone(), o[2].clone(), o[3].clone()]);
        let _ = o5;
        s.on_fill(&mut ctx, &fill("o-5", Side::Sell, dec!(1.05157), "100"));
        assert_eq!(bar(s.as_mut(), &mut ctx, 2, "101"), vec![buy("95")]);
    }

    /// The same buy fills in two parts, 0.5 then 0.55263 BTC: nothing until
    /// the order is done, then one sell of the whole 1.05157737 (lot
    /// 1.05157).
    #[test]
    fn a_split_buy_sells_once_it_is_done() {
        let (mut s, mut ctx) = start(params(""));
        let opening = bar(s.as_mut(), &mut ctx, 0, "100");
        let o = accept(s.as_mut(), &mut ctx, &opening, 1);
        driver::set_open_orders(&mut ctx, o.clone());
        s.on_fill(&mut ctx, &fill("o-2", Side::Buy, dec!(0.5), "95"));
        assert!(bar(s.as_mut(), &mut ctx, 1, "95").is_empty());
        driver::set_open_orders(&mut ctx, vec![o[0].clone(), o[2].clone(), o[3].clone()]);
        s.on_fill(&mut ctx, &fill("o-2", Side::Buy, dec!(0.55263), "95"));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "96"),
            vec![sell("100", dec!(1.05157))]
        );
    }

    /// The 95 buy is refused (out of cash): the gap is empty again and the
    /// grid places nothing until a fill. The 100 buy then fills (1 BTC,
    /// 0.999 net): its sell goes up at 105 and the 95 buy is retried.
    #[test]
    fn a_refused_order_pauses_until_a_fill() {
        let (mut s, mut ctx) = start(params(""));
        let opening = bar(s.as_mut(), &mut ctx, 0, "100");
        let o1 = order(&opening[0], "o-1");
        s.on_order(&mut ctx, &o1);
        s.on_rejected(&mut ctx, &opening[1], "venue error -2010");
        let o3 = order(&opening[2], "o-3");
        let o4 = order(&opening[3], "o-4");
        s.on_order(&mut ctx, &o3);
        s.on_order(&mut ctx, &o4);
        driver::set_open_orders(&mut ctx, vec![o1.clone(), o3, o4.clone()]);
        assert!(bar(s.as_mut(), &mut ctx, 1, "100").is_empty());

        driver::set_open_orders(&mut ctx, vec![o1, o4]);
        s.on_fill(&mut ctx, &fill("o-3", Side::Buy, dec!(1), "100"));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "100"),
            vec![buy("95"), sell("105", dec!(0.999))]
        );
    }

    /// stop_price 85, close 84: cancel every order; while a cancel is in
    /// flight nothing more; once idle, sell all 2 BTC held at market, then
    /// stop for good.
    #[test]
    fn below_the_stop_it_cancels_sells_everything_and_stops() {
        let (mut s, mut ctx) = start(params(r#"stop_price = "85""#));
        let opening = bar(s.as_mut(), &mut ctx, 0, "100");
        let o = accept(s.as_mut(), &mut ctx, &opening, 1);
        driver::set_open_orders(&mut ctx, o.clone());
        let ids: Vec<Signal> = o.iter().map(|o| Signal::Cancel(o.id.clone())).collect();
        assert_eq!(bar(s.as_mut(), &mut ctx, 1, "84"), ids);
        driver::set_open_orders(&mut ctx, vec![]);
        driver::set_cancelling_orders(&mut ctx, o);
        assert!(bar(s.as_mut(), &mut ctx, 2, "84").is_empty());
        driver::set_cancelling_orders(&mut ctx, vec![]);
        hold(&mut ctx, dec!(2));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 3, "84"),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(2))),
                kind: OrderKind::Market,
            }]
        );
        assert!(bar(s.as_mut(), &mut ctx, 4, "100").is_empty());
    }

    /// Fresh start holding 1 BTC at 100: the gap above the price (100 to
    /// 105) takes its lot, lot(100/100) * 0.999 = 0.999, sold at 105; the
    /// 0.001 left can't fill the next gap's sell at 110 (0.11 USDT < 5),
    /// so it's dust and that gap buys at 105.
    #[test]
    fn a_fresh_start_gives_held_coin_to_the_gaps_above() {
        let (mut s, mut ctx) = start(params(""));
        hold(&mut ctx, dec!(1));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "100"),
            vec![buy("90"), buy("95"), sell("105", dec!(0.999)), buy("105")]
        );
        assert_eq!(dust(s.as_ref()), dec!(0.001));
    }

    fn saved(gaps: serde_json::Value, dust: &str) -> serde_json::Value {
        serde_json::json!({ "v": 1, "stage": "running", "started": true,
                            "gaps": gaps, "dust": dust })
    }

    /// Saved with buys working at 90 and 95; the books now hold 1 BTC more
    /// (a buy filled while down). The nearest one, 95, takes it: up to its
    /// lot lot(100/95) * 0.999 = 1.05157737, so all of the 1.
    #[test]
    fn a_buy_filled_while_down_is_credited_to_the_nearest_gap() {
        let (mut s, mut ctx) = start(params(""));
        hold(&mut ctx, dec!(1));
        let state = saved(
            serde_json::json!([{"buying": {"got": "0"}}, {"buying": {"got": "0"}}, "empty", "empty"]),
            "0",
        );
        s.restore_state(state, &ctx).unwrap();
        assert_eq!(
            gaps(s.as_ref()),
            serde_json::json!(["empty", {"held": {"qty": "1"}}, "empty", "empty"])
        );
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "96"),
            vec![buy("90"), sell("100", dec!(1)), buy("100"), buy("105")]
        );
    }

    /// Saved selling 1.05157 at 100 with 0.00000737 dust; the books hold
    /// only the dust: the sell filled while down, so the gap is empty.
    #[test]
    fn a_sell_filled_while_down_empties_its_gap() {
        let (mut s, mut ctx) = start(params(""));
        hold(&mut ctx, dec!(0.00000737));
        let state = saved(
            serde_json::json!(["empty", {"selling": {"qty": "1.05157", "sold": "0"}}, "empty", "empty"]),
            "0.00000737",
        );
        s.restore_state(state, &ctx).unwrap();
        assert_eq!(
            gaps(s.as_ref()),
            serde_json::json!(["empty", "empty", "empty", "empty"])
        );
    }

    /// Saved holding 1 BTC in a gap with nothing working, but the books
    /// are flat: no fill explains it, so the state is refused and the
    /// strategy is as it was.
    #[test]
    fn state_the_books_contradict_is_refused() {
        let (mut s, mut ctx) = start(params(""));
        hold(&mut ctx, dec!(0));
        let before = s.save_state();
        let state = saved(
            serde_json::json!([{"held": {"qty": "1"}}, "empty", "empty", "empty"]),
            "0",
        );
        assert!(s.restore_state(state, &ctx).is_err());
        assert_eq!(s.save_state(), before);
    }

    /// Nothing working and the books agree: restored exactly.
    #[test]
    fn idle_state_round_trips() {
        let (mut s, mut ctx) = start(params(""));
        hold(&mut ctx, dec!(1.00001));
        let state = saved(
            serde_json::json!(["empty", {"held": {"qty": "1"}}, "empty", "empty"]),
            "0.00001",
        );
        s.restore_state(state.clone(), &ctx).unwrap();
        assert_eq!(s.save_state(), Some(state));
    }
}
