//! `funding_carry`: cash-and-carry on a perpetual's funding.
//!
//! Long the spot coin, short the same quantity of its USDⓈ-M perpetual:
//! the price moves cancel, and while funding is positive (longs pay shorts,
//! the usual state in a long-biased market) the short collects it. A carry,
//! not a forecast. Measured on Binance BTCUSDT from June 2021 to November
//! 2026: the funding a short received summed to 7.2% a year of the hedged
//! notional (2.9–12.0% by calendar year, positive every year; 76–92% of
//! settlements positive), against about 0.4% per round trip in taker fees
//! and slippage.
//!
//! - **enter** (flat): the mean of the last `window` funding rates,
//!   annualized by their spacing, is at least `enter_apr` → buy `q` spot
//!   and sell `q` perp at market, `q` = the smallest of `notional`, what
//!   the spot cash can buy (`spot_capital` − cost still held + realized,
//!   less the fee, 1% spare) and what the futures wallet can margin
//!   (`perp_capital` + realized, fees and funding included, ×
//!   `leverage` × 0.9), over the close, on the perp's lot. The strategy
//!   can't see the wallets (invariant 5): these are its own books'
//!   estimates, and the broker still refuses what doesn't fit. Without
//!   them, a guard exit that drained the futures wallet left every later
//!   entry's short refused while its spot buy filled alone, closed the
//!   next bar, at a fee each time (190 times in a 2023–2026 backtest).
//! - **exit** (hedged): that mean falls below `exit_apr`, or the perp's
//!   close has risen `max_rise` above the short's entry → close the short
//!   (reduce-only) and sell the spot. The second rule is the margin guard:
//!   the spot gain sits in the spot wallet and the short's loss in the
//!   futures wallet, and nothing moves money between them, so a long rally
//!   would otherwise liquidate the short (at 2x isolated, about +50%).
//! - **one leg only** (the other refused or liquidated): close it. A
//!   refused order backs off entries (`portfolio::Backoff`: 2^k − 1
//!   evaluations after k refusals in a row), so a partial entry can't loop.
//!
//! Decides on the perp's candle once both coins' candles of that close are
//! in and nothing is working on either. The spot buy's base fee leaves the
//! long a fee's worth smaller than the short (0.1%: a small short bias).
//!
//! Capital: the spot leg ties up the full notional and the short its
//! margin (notional / leverage), so the carry on capital is lower than on
//! notional (2x: 7.2% → about 4.8%). Risks: funding turning negative for
//! weeks (exit_apr), a rally faster than the guard, the venue itself.
//! Edge measured on one coin's history only; unverified live.
//!
//! State: the funding window (time and rate per settlement), saved and
//! restored, since a live feed delivers funding only from its start.
//! Positions are rebuilt from the books.

use super::portfolio::{self, Backoff};
use super::rebalance::completes;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, InstrumentId, MarketEvent, Notional, OrderKind, Qty, Side, Signal, Size,
};
use serde::Deserialize;
use std::collections::VecDeque;

const NAME: &str = "funding_carry";
const YEAR_MS: i64 = 365 * 86_400_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FundingCarryParams {
    /// The spot leg (`"binance:BTCUSDT"`).
    spot: InstrumentId,
    /// The perpetual hedged against it (`"binance-usdm:BTCUSDT"`).
    perp: InstrumentId,
    /// Quote per leg at entry, at most (`"1000"`): notional, not margin.
    notional: Notional,
    /// The spot cash this strategy trades with (`"1000"`, USDT).
    spot_capital: Notional,
    /// The futures wallet it trades with (`"500"`, USDT).
    perp_capital: Notional,
    /// The leverage the short is margined at; must match the broker's
    /// (`--leverage`, `[perp] leverage`). Sizing only.
    leverage: u32,
    /// Funding settlements averaged (`21`: a week at 8 h); 2 to 500.
    window: usize,
    /// Annualized mean funding that opens the hedge (`"0.08"` = 8%).
    #[serde(deserialize_with = "exact_decimal")]
    enter_apr: Decimal,
    /// Annualized mean funding below which it closes (`"0.02"`); below
    /// `enter_apr`.
    #[serde(deserialize_with = "exact_decimal")]
    exit_apr: Decimal,
    /// Close when the perp has risen this fraction above the short's entry
    /// (`"0.3"`), before its margin runs out. In (0, 1].
    #[serde(deserialize_with = "exact_decimal")]
    max_rise: Decimal,
    /// Fee per trade assumed; required, range-checked, unused in decisions.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed (bps); required, unused.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct FundingCarry {
    p: FundingCarryParams,
    /// The newest `window` settlements: (time ms, rate), oldest first.
    rates: VecDeque<(i64, Decimal)>,
    /// Entries skipped after refusals (not saved: a restart retries).
    backoff: Backoff,
}

impl FundingCarry {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: FundingCarryParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if p.spot == p.perp {
            return bad("spot and perp must differ");
        }
        if !p.notional.is_positive()
            || !p.spot_capital.is_positive()
            || !p.perp_capital.is_positive()
        {
            return bad("notional, spot_capital and perp_capital must be positive");
        }
        if p.leverage == 0 || p.leverage > 125 {
            return bad("leverage must be 1 to 125");
        }
        if !(2..=500).contains(&p.window) {
            return bad("window must be 2 to 500");
        }
        if p.enter_apr <= Decimal::ZERO || p.exit_apr >= p.enter_apr {
            return bad("need enter_apr > 0 and exit_apr < enter_apr");
        }
        if p.max_rise <= Decimal::ZERO || p.max_rise > Decimal::ONE {
            return bad("max_rise must be in (0, 1]");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self {
            p,
            rates: VecDeque::new(),
            backoff: Backoff::default(),
        }))
    }

    /// Mean funding rate of a full window, times settlements per year (from
    /// the window's own spacing); `None` until the window is full.
    fn apr(&self) -> Option<Decimal> {
        if self.rates.len() < self.p.window {
            return None;
        }
        let (first, last) = (self.rates.front()?.0, self.rates.back()?.0);
        let span = last - first;
        if span <= 0 {
            return None;
        }
        let n = Decimal::from(self.rates.len());
        let mean = self.rates.iter().map(|(_, r)| *r).sum::<Decimal>() / n;
        let per_year = Decimal::from(YEAR_MS) * (n - Decimal::ONE) / Decimal::from(span);
        Some(mean * per_year)
    }

    /// Quote one leg may take: `notional`, capped by the spot cash and the
    /// futures wallet as this strategy's books put them (module docs).
    fn affordable(&self, ctx: &Context) -> Decimal {
        let s = ctx.position(&self.p.spot);
        let spot_cash = self.p.spot_capital.get() - (s.cost_basis().get() - s.realized_pnl().get());
        let wallet = self.p.perp_capital.get() + ctx.position(&self.p.perp).realized_pnl().get();
        let spot_cap = spot_cash * (Decimal::ONE - self.p.fee) * Decimal::new(99, 2);
        let perp_cap = wallet * Decimal::from(self.p.leverage) * Decimal::new(9, 1);
        self.p.notional.get().min(spot_cap).min(perp_cap)
    }

    fn decide(&mut self, ctx: &Context, c: &Candle) -> Vec<Signal> {
        let (spot, perp) = (&self.p.spot, &self.p.perp);
        if c.instrument != *perp
            || !completes(ctx, c, [spot, perp].into_iter())
            || !portfolio::idle(ctx, [spot, perp])
        {
            return Vec::new();
        }
        let (Some(si), Some(pi), Some(sc)) = (
            ctx.instrument(spot),
            ctx.instrument(perp),
            ctx.last_candle(spot),
        ) else {
            return Vec::new();
        };
        // Spot held, if a sale of it would pass the filters (else dust).
        let s_units = si.round_qty(ctx.position(spot).qty());
        let long = si
            .check_market(s_units, sc.close, Side::Sell)
            .is_ok()
            .then_some(s_units);
        // The perp: a short to buy back, or a long (never ours) to sell.
        let pp = ctx.position(perp);
        let p_units = pi.round_qty(Qty::new(pp.qty().get().abs()));
        let p_side = if pp.qty().is_negative() {
            Side::Buy
        } else {
            Side::Sell
        };
        let hedge = pi
            .check_market(p_units, c.close, p_side)
            .is_ok()
            .then_some((p_units, p_side));
        let close_perp = |(q, side): (Qty, Side)| Signal::Reduce {
            instrument: perp.clone(),
            side,
            qty: q,
            kind: OrderKind::Market,
        };
        let sell_spot = |q: Qty| Signal::Submit {
            instrument: spot.clone(),
            side: Side::Sell,
            size: Size::Base(q),
            kind: OrderKind::Market,
        };
        let apr = self.apr();
        match (long, hedge) {
            (None, None) => {
                if !apr.is_some_and(|a| a >= self.p.enter_apr) || self.backoff.hold() {
                    return Vec::new();
                }
                let budget = self.affordable(ctx);
                if budget <= Decimal::ZERO {
                    return Vec::new();
                }
                let Ok(q) = pi.qty_for(Notional::new(budget), c.close) else {
                    return Vec::new();
                };
                let q = pi.round_qty(q);
                if si.check_market(q, sc.close, Side::Buy).is_err()
                    || pi.check_market(q, c.close, Side::Sell).is_err()
                {
                    return Vec::new();
                }
                vec![
                    Signal::Submit {
                        instrument: spot.clone(),
                        side: Side::Buy,
                        size: Size::Base(q),
                        kind: OrderKind::Market,
                    },
                    Signal::Submit {
                        instrument: perp.clone(),
                        side: Side::Sell,
                        size: Size::Base(q),
                        kind: OrderKind::Market,
                    },
                ]
            }
            (Some(s), Some((q, Side::Buy))) => {
                // Entry price of the short: |basis| / |qty|.
                let qty = pp.qty().get().abs();
                let entry = if qty.is_zero() {
                    c.close.get()
                } else {
                    pp.cost_basis().get().abs() / qty
                };
                let risen = c.close.get() >= entry * (Decimal::ONE + self.p.max_rise);
                let faded = apr.is_some_and(|a| a < self.p.exit_apr);
                if risen || faded {
                    vec![close_perp((q, Side::Buy)), sell_spot(s)]
                } else {
                    Vec::new()
                }
            }
            // One leg alone, or a perp long that was never this strategy's
            // hedge: close what's there.
            (Some(s), Some(h)) => vec![close_perp(h), sell_spot(s)],
            (Some(s), None) => vec![sell_spot(s)],
            (None, Some(h)) => vec![close_perp(h)],
        }
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: FundingCarry::build }
}

impl Strategy for FundingCarry {
    fn name(&self) -> &str {
        NAME
    }

    fn on_event(&mut self, ctx: &mut Context, e: &MarketEvent) -> Vec<Signal> {
        match e {
            MarketEvent::Funding {
                instrument,
                rate,
                ts,
                ..
            } if *instrument == self.p.perp => {
                self.rates.push_back((ts.as_millis(), *rate));
                while self.rates.len() > self.p.window {
                    self.rates.pop_front();
                }
                Vec::new()
            }
            MarketEvent::Candle(c) => self.decide(ctx, c),
            _ => Vec::new(),
        }
    }

    /// Something filled: the venue takes orders; stop backing off.
    fn on_fill(&mut self, _ctx: &mut Context, _f: &bot_core::Fill) {
        self.backoff.filled();
    }

    /// A refused leg (margin, balance): wait longer before entering again.
    fn on_rejected(&mut self, ctx: &mut Context, _s: &Signal, reason: &str) {
        tracing::warn!(
            strategy = NAME,
            reason,
            "order refused; backing off entries"
        );
        self.backoff.refused(ctx.now());
    }

    /// The funding window, rates as strings (exact).
    fn save_state(&self) -> Option<serde_json::Value> {
        let rates: Vec<serde_json::Value> = self
            .rates
            .iter()
            .map(|(t, r)| serde_json::json!([t, r.to_string()]))
            .collect();
        Some(serde_json::json!({ "v": super::STATE_VERSION, "rates": rates }))
    }

    /// Takes the window as saved (the newest `window` of it).
    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::check_version(NAME, &state)?;
        let bad = || super::state_error(NAME, "bad rates");
        let mut rates = VecDeque::new();
        for r in state["rates"].as_array().ok_or_else(bad)? {
            let t = r[0].as_i64().ok_or_else(bad)?;
            let rate = r[1]
                .as_str()
                .and_then(|s| Decimal::from_str_exact(s).ok())
                .ok_or_else(bad)?;
            rates.push_back((t, rate));
        }
        while rates.len() > self.p.window {
            rates.pop_front();
        }
        self.rates = rates;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{
        AssetClass, CandleSpec, Instrument, InstrumentSpec, Position, Price, Timestamp,
    };
    use rust_decimal_macros::dec;

    const HOUR: i64 = 3_600_000;

    fn perp() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance-usdm:BTCUSDT".parse().unwrap(),
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap()
    }

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"spot = "binance:BTCUSDT"
               perp = "binance-usdm:BTCUSDT"
               notional = "1000"
               spot_capital = "10000"
               perp_capital = "10000"
               leverage = 1
               window = 3
               enter_apr = "0.1"
               exit_apr = "0.02"
               max_rise = "0.3"
               fee = "0.001"
               slippage_bps = "5"
               {extra}"#
        ))
        .unwrap()
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt(), perp()], Params::default(), 10)
    }

    fn funding(s: &mut dyn Strategy, ctx: &mut Context, h: i64, rate: Decimal) {
        let e = MarketEvent::Funding {
            instrument: perp().id.clone(),
            rate,
            mark: Price::new(dec!(100)),
            ts: Timestamp::from_millis(h * HOUR),
        };
        assert!(s.on_event(ctx, &e).is_empty());
    }

    /// Spot then perp candle of hour `i` at `price`; the perp's signals.
    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let spot = candle(i, price);
        let p = Candle::new(CandleSpec {
            instrument: perp().id.clone(),
            ..spot.clone().into()
        })
        .unwrap();
        driver::set_now(ctx, p.close_time);
        driver::push_candle(ctx, spot.clone());
        assert!(s.on_event(ctx, &MarketEvent::Candle(spot)).is_empty());
        driver::push_candle(ctx, p.clone());
        s.on_event(ctx, &MarketEvent::Candle(p))
    }

    fn hold(ctx: &mut Context, id: &InstrumentId, qty: Decimal, basis: Decimal) {
        driver::set_position(
            ctx,
            Position::from_parts(
                id.clone(),
                Qty::new(qty),
                Notional::new(basis),
                Notional::ZERO,
            )
            .unwrap(),
        );
    }

    fn submit(id: &InstrumentId, side: Side, q: Decimal) -> Signal {
        Signal::Submit {
            instrument: id.clone(),
            side,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        }
    }

    fn reduce(side: Side, q: Decimal) -> Signal {
        Signal::Reduce {
            instrument: perp().id.clone(),
            side,
            qty: Qty::new(q),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
        for (from, to) in [
            ("window = 3", "window = 1"),
            ("exit_apr = \"0.02\"", "exit_apr = \"0.1\""),
            ("enter_apr = \"0.1\"", "enter_apr = \"0\""),
            ("max_rise = \"0.3\"", "max_rise = \"0\""),
            (
                "perp = \"binance-usdm:BTCUSDT\"",
                "perp = \"binance:BTCUSDT\"",
            ),
        ] {
            let raw = r#"spot = "binance:BTCUSDT"
perp = "binance-usdm:BTCUSDT"
notional = "1000"
spot_capital = "10000"
perp_capital = "10000"
leverage = 1
window = 3
enter_apr = "0.1"
exit_apr = "0.02"
max_rise = "0.3"
fee = "0.001"
slippage_bps = "5""#
                .replace(from, to);
            assert!(
                build(NAME, &Params::from_toml(&raw).unwrap()).is_err(),
                "{to}"
            );
        }
    }

    /// Three settlements of 0.0001, 8 h apart: mean 0.0001 × (365 days /
    /// 8 h = 1,095 a year) = 10.95% ≥ 10% → buy 10 spot and sell 10 perp
    /// (1,000 / 100). Two settlements only: no window, nothing.
    #[test]
    fn opens_the_hedge_once_a_full_window_pays_enough() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        funding(s.as_mut(), &mut ctx, 0, dec!(0.0001));
        funding(s.as_mut(), &mut ctx, 8, dec!(0.0001));
        assert!(bar(s.as_mut(), &mut ctx, 9, "100").is_empty(), "2 of 3");
        funding(s.as_mut(), &mut ctx, 16, dec!(0.0001));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 17, "100"),
            vec![
                submit(&btcusdt().id, Side::Buy, dec!(10)),
                submit(&perp().id, Side::Sell, dec!(10)),
            ]
        );
        // 0.00009 × 1,095 = 9.855% < 10%: no entry.
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = self::ctx();
        for h in [0, 8, 16] {
            funding(s.as_mut(), &mut ctx, h, dec!(0.00009));
        }
        assert!(bar(s.as_mut(), &mut ctx, 17, "100").is_empty());
    }

    /// Hedged 10 / −10 from 100. Funding turns: −0.0001 three times → mean
    /// −10.95% < 2% → close the short (reduce-only) and sell the spot.
    /// Separately, still paying, but the perp at 130 = 100 · 1.3: the
    /// margin guard closes both.
    #[test]
    fn closes_when_funding_fades_or_the_price_runs_up() {
        let hedged = |ctx: &mut Context| {
            hold(ctx, &btcusdt().id, dec!(10), dec!(1000));
            hold(ctx, &perp().id, dec!(-10), dec!(-1000));
        };
        let close = vec![
            reduce(Side::Buy, dec!(10)),
            submit(&btcusdt().id, Side::Sell, dec!(10)),
        ];
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        hedged(&mut ctx);
        for h in [0, 8, 16] {
            funding(s.as_mut(), &mut ctx, h, dec!(0.0001));
        }
        assert!(
            bar(s.as_mut(), &mut ctx, 17, "100").is_empty(),
            "paying: hold"
        );
        assert!(
            bar(s.as_mut(), &mut ctx, 18, "129.99").is_empty(),
            "under the guard"
        );
        assert_eq!(bar(s.as_mut(), &mut ctx, 19, "130"), close, "guard");

        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = self::ctx();
        hedged(&mut ctx);
        for h in [0, 8, 16] {
            funding(s.as_mut(), &mut ctx, h, dec!(-0.0001));
        }
        assert_eq!(bar(s.as_mut(), &mut ctx, 17, "100"), close, "faded");
    }

    /// A leg alone (its partner refused, or liquidated) is closed.
    #[test]
    fn a_lone_leg_is_closed() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, &btcusdt().id, dec!(10), dec!(1000));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "100"),
            vec![submit(&btcusdt().id, Side::Sell, dec!(10))]
        );
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = self::ctx();
        hold(&mut ctx, &perp().id, dec!(-10), dec!(-1000));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 0, "100"),
            vec![reduce(Side::Buy, dec!(10))]
        );
    }

    /// The window survives a restart: saved after three settlements,
    /// restored into a fresh strategy, it opens the hedge as the original
    /// would.
    #[test]
    fn the_funding_window_survives_a_restart() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        for h in [0, 8, 16] {
            funding(s.as_mut(), &mut ctx, h, dec!(0.0001));
        }
        let state = s.save_state().unwrap();
        assert_eq!(
            state,
            serde_json::json!({ "v": 1, "rates": [
                [0, "0.0001"], [8 * HOUR, "0.0001"], [16 * HOUR, "0.0001"]
            ]})
        );
        let mut fresh = build(NAME, &params("")).unwrap();
        fresh.restore_state(state, &ctx).unwrap();
        assert_eq!(bar(fresh.as_mut(), &mut ctx, 17, "100").len(), 2);
        assert!(
            fresh
                .restore_state(serde_json::json!({ "v": 1, "rates": [[0, 1]] }), &ctx)
                .is_err(),
            "rate as a number"
        );
    }

    /// Sized to the wallets as the books put them: perp_capital 500,
    /// leverage 2, the short lost 300 earlier (realized −300) → wallet
    /// 200 · 2 · 0.9 = 360 of margin capacity < notional 1,000 and the spot
    /// side's 10,000 · 0.999 · 0.99 → q = 360 / 100 = 3.6.
    #[test]
    fn entries_fit_both_wallets() {
        let p = params("").parse::<toml::Table>("t").unwrap();
        let mut t = p.clone();
        t.insert("perp_capital".into(), toml::Value::String("500".into()));
        t.insert("leverage".into(), toml::Value::Integer(2));
        let mut s = build(NAME, &Params::new(t)).unwrap();
        let mut ctx = ctx();
        driver::set_position(
            &mut ctx,
            Position::from_parts(
                perp().id.clone(),
                Qty::ZERO,
                Notional::ZERO,
                Notional::new(dec!(-300)),
            )
            .unwrap(),
        );
        for h in [0, 8, 16] {
            funding(s.as_mut(), &mut ctx, h, dec!(0.0001));
        }
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 17, "100"),
            vec![
                submit(&btcusdt().id, Side::Buy, dec!(3.6)),
                submit(&perp().id, Side::Sell, dec!(3.6)),
            ]
        );
    }

    /// After a refusal the next entry waits one evaluation (2^1 − 1).
    #[test]
    fn a_refusal_backs_off_the_next_entry() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = ctx();
        for h in [0, 8, 16] {
            funding(s.as_mut(), &mut ctx, h, dec!(0.0001));
        }
        driver::set_now(&mut ctx, Timestamp::from_millis(17 * HOUR));
        let sig = submit(&perp().id, Side::Sell, dec!(10));
        s.on_rejected(&mut ctx, &sig, "insufficient margin");
        assert!(bar(s.as_mut(), &mut ctx, 17, "100").is_empty(), "held off");
        assert_eq!(bar(s.as_mut(), &mut ctx, 18, "100").len(), 2, "then tries");
    }
}
