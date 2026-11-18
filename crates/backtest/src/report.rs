//! Backtest report: P&L, drawdown, trade stats and Sharpe, all `Decimal`
//! (invariant 1).
//!
//! The report keeps its own cash ledger, independent of the paper broker's
//! balance: it replays the fills, in order, against the candle stream, so
//! a broker that let an overspend through still shows up here.
//! Equity (cash plus holdings marked at each instrument's latest close) is
//! taken once per close time, after every candle and fill up to it.
//!
//! A perpetual buys nothing outright: its fills move "cash" (the wallet)
//! only by realized P&L and fees, funding settlements (`MarketEvent::
//! Funding`) by what they pay, and a settlement counts before every fill
//! settled at the first close time at or after it, as the broker sees it
//! (the feed delivers it before that candle, whose fills, stamped at its
//! open, may precede it on the clock: Binance stamps funding ms after the
//! hour), and equity counts its unrealized P&L at the latest
//! close (qty × close − entry notional), not its notional. A closed trade
//! is any fill that shrinks the position, liquidations included.

use bot_core::{
    AssetClass, Candle, Decimal, Fee, Fill, Instrument, InstrumentId, Interval, MarketEvent,
    Notional, Position, Price, Side,
};
use engine::RunReport;
use rust_decimal::MathematicalOps;
use std::collections::HashMap;
use std::fmt;

/// Results of one backtest. Model output under the fill model's stated
/// assumptions, never a promise of live performance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub initial_cash: Notional,
    /// Cash plus holdings marked at the last close.
    pub final_equity: Notional,
    /// Profit closed out by sells, after fees (average-cost accounting).
    pub realized_pnl: Notional,
    /// Lowest cash balance after the fills of any one instant, starting at
    /// `initial_cash`. Fills sharing a timestamp are simultaneous (one
    /// open, several coins), so cash is read after all of them: the order
    /// the backtest replays coins in must not decide the figure. The
    /// broker refuses buys it can't cover,
    /// so this stays at or above zero; negative would mean quote spent
    /// that wasn't there, a broker bug this independent ledger exists to
    /// expose.
    pub min_cash: Notional,
    /// Largest fall from a running equity peak, as a fraction of that peak
    /// (`0.25` = 25%). The peak starts at `initial_cash`.
    pub max_drawdown: Decimal,
    /// Executions, buys and sells.
    pub fills: usize,
    /// Sells, each closing (part of) a position and realizing P&L.
    pub closed_trades: usize,
    /// Share of closed trades with positive realized P&L; `None` if none
    /// closed.
    pub win_rate: Option<Decimal>,
    /// Annualized Sharpe of per-bar equity returns, risk-free rate 0,
    /// sample standard deviation, 24/7 year (8760 bars at 1h). `None` with
    /// fewer than two returns or zero volatility.
    pub sharpe: Option<Decimal>,
    pub rejected_signals: usize,
    /// The earn assumption the run was valued with, if any.
    pub earn: Option<EarnModel>,
    /// Interest from it, included in equity (simple, not reinvested).
    pub earn_interest: Notional,
    /// Perpetual funding received (negative: paid), in realized P&L.
    pub funding: Notional,
    /// Equity after each close time, oldest first: the series drawdown
    /// and Sharpe are computed from (for charts).
    pub equity_curve: Vec<(bot_core::Timestamp, Notional)>,
    /// Why the engine's kill switch ended the run early, if it did.
    pub killed: Option<String>,
}

/// The share of cash a run keeps in Simple Earn (`[earn] ratio`) and the
/// yearly rate assumed for it. Binance's rates move daily and their
/// history isn't published, so the rate is an assumption the backtest
/// states, never a measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EarnModel {
    /// In [0, 1).
    pub ratio: Decimal,
    /// Annual rate as a fraction (`0.05` = 5%), simple interest.
    pub apr: Decimal,
}

/// Milliseconds in a 365-day year: the APR's year.
const YEAR_MS: i64 = 365 * 86_400_000;

impl Report {
    /// Builds the report from a finished run and the candles it replayed.
    pub fn compute(
        run: &RunReport,
        candles: &[Candle],
        instruments: &[Instrument],
        initial_cash: Notional,
        interval: Interval,
    ) -> Self {
        Self::compute_with_earn(run, candles, instruments, initial_cash, interval, None)
    }

    /// [`compute`](Self::compute), plus interest on the share of cash kept
    /// in Simple Earn: between two close times the earn share of the cash
    /// the ledger held after the earlier one's fills earns `apr` for the
    /// time between them. Simple interest, never reinvested or spent: it
    /// adds to equity (and so to drawdown and Sharpe), not to cash.
    pub fn compute_with_earn(
        run: &RunReport,
        candles: &[Candle],
        instruments: &[Instrument],
        initial_cash: Notional,
        interval: Interval,
        earn: Option<EarnModel>,
    ) -> Self {
        Self::compute_full(run, candles, &[], instruments, initial_cash, interval, earn)
    }

    /// [`compute_with_earn`](Self::compute_with_earn) with the run's other
    /// market events: funding settlements among them are booked (module
    /// docs); the rest are ignored.
    pub fn compute_full(
        run: &RunReport,
        candles: &[Candle],
        events: &[MarketEvent],
        instruments: &[Instrument],
        initial_cash: Notional,
        interval: Interval,
        earn: Option<EarnModel>,
    ) -> Self {
        let by_id: HashMap<&InstrumentId, &Instrument> =
            instruments.iter().map(|i| (&i.id, i)).collect();
        let mut sorted: Vec<&Candle> = candles.iter().collect();
        sorted.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));

        let mut ledger = Ledger {
            cash: initial_cash,
            min_cash: initial_cash,
            positions: HashMap::new(),
            marks: HashMap::new(),
            closed: 0,
            wins: 0,
            funding: Notional::ZERO,
        };
        let mut funding: Vec<(bot_core::Timestamp, &InstrumentId, Decimal, Price)> = events
            .iter()
            .filter_map(|e| match e {
                MarketEvent::Funding {
                    instrument,
                    rate,
                    mark,
                    ts,
                } => Some((*ts, instrument, *rate, *mark)),
                _ => None,
            })
            .collect();
        funding.sort_by_key(|f| f.0);
        let mut funding = funding.into_iter().peekable();
        let mut fills = run.fills.iter().peekable();
        let mut equity = Vec::new();
        let mut curve = Vec::new();
        let mut interest = Notional::ZERO;
        let mut prev: Option<bot_core::Timestamp> = None;
        let mut i = 0;
        while i < sorted.len() {
            let t = sorted[i].close_time;
            if let (Some(e), Some(p)) = (earn, prev) {
                let years = Decimal::from(t.as_millis() - p.as_millis()) / Decimal::from(YEAR_MS);
                interest += ledger.cash * (e.ratio * e.apr * years);
            }
            prev = Some(t);
            // Funding up to this close came before its candle (module docs).
            while let Some((_, id, rate, mark)) = funding.next_if(|f| f.0 <= t) {
                ledger.fund(id, rate, mark);
                ledger.instant_done();
            }
            while let Some(f) = fills.next_if(|f| f.ts <= t) {
                if let Some(inst) = by_id.get(&f.instrument) {
                    ledger.apply(f, inst);
                }
                if fills.peek().is_none_or(|next| next.ts != f.ts) {
                    ledger.instant_done();
                }
            }
            while i < sorted.len() && sorted[i].close_time == t {
                ledger
                    .marks
                    .insert(sorted[i].instrument.clone(), sorted[i].close);
                i += 1;
            }
            equity.push(ledger.equity(&by_id) + interest);
            curve.push(t);
        }
        // Fills after the last candle can't happen (fills need a candle),
        // but apply any so the ledger is complete.
        for f in fills {
            if let Some(inst) = by_id.get(&f.instrument) {
                ledger.apply(f, inst);
            }
        }
        ledger.instant_done();

        let realized_pnl = ledger.positions.values().map(Position::realized_pnl).sum();
        let final_equity = ledger.equity(&by_id) + interest;
        let win_rate =
            (ledger.closed > 0).then(|| Decimal::from(ledger.wins) / Decimal::from(ledger.closed));
        Self {
            initial_cash,
            final_equity,
            realized_pnl,
            min_cash: ledger.min_cash,
            max_drawdown: max_drawdown(initial_cash, &equity),
            fills: run.fills.len(),
            closed_trades: ledger.closed,
            win_rate,
            sharpe: sharpe(&equity, interval),
            rejected_signals: run.rejections.len(),
            earn,
            earn_interest: interest,
            funding: ledger.funding,
            equity_curve: curve.into_iter().zip(equity).collect(),
            killed: run.killed.clone(),
        }
    }
}

struct Ledger {
    cash: Notional,
    min_cash: Notional,
    positions: HashMap<InstrumentId, Position>,
    marks: HashMap<InstrumentId, Price>,
    closed: usize,
    wins: usize,
    funding: Notional,
}

impl Ledger {
    fn fund(&mut self, id: &InstrumentId, rate: Decimal, mark: Price) {
        if let Some(p) = self.positions.get_mut(id) {
            let paid = p.apply_funding(rate, mark);
            self.cash += paid;
            self.funding += paid;
        }
    }

    fn apply(&mut self, f: &Fill, inst: &Instrument) {
        if inst.class == AssetClass::Perpetual {
            let pos = self
                .positions
                .entry(f.instrument.clone())
                .or_insert_with(|| Position::flat(f.instrument.clone()));
            let (before, held) = (pos.realized_pnl(), pos.qty().get());
            if pos.apply(f, inst).is_err() {
                return;
            }
            let realized = pos.realized_pnl() - before;
            self.cash += realized;
            // Against an open position: closes (part of) it.
            let closes = !held.is_zero() && held.is_sign_positive() == (f.side == Side::Sell);
            if closes {
                self.closed += 1;
                if realized.is_positive() {
                    self.wins += 1;
                }
            }
            return;
        }
        let notional = inst.notional(f.qty, f.price);
        let quote_fee = match &f.fee {
            Fee::Quote(n) => *n,
            Fee::Base(_) | Fee::Other { .. } => Notional::ZERO,
        };
        let pos = self
            .positions
            .entry(f.instrument.clone())
            .or_insert_with(|| Position::flat(f.instrument.clone()));
        let before = pos.realized_pnl();
        // The broker already applied this fill to its own position, so it
        // can't fail here; if it somehow does, cash is left untouched too.
        if pos.apply(f, inst).is_err() {
            return;
        }
        match f.side {
            Side::Buy => self.cash -= notional + quote_fee,
            Side::Sell => {
                self.cash += notional - quote_fee;
                self.closed += 1;
                if pos.realized_pnl() > before {
                    self.wins += 1;
                }
            }
        }
    }

    /// Every fill of one instant is applied: cash as of now counts.
    fn instant_done(&mut self) {
        self.min_cash = self.min_cash.min(self.cash);
    }

    fn equity(&self, by_id: &HashMap<&InstrumentId, &Instrument>) -> Notional {
        let held: Notional = self
            .positions
            .iter()
            .filter_map(|(id, p)| {
                let mark = self.marks.get(id)?;
                let inst = by_id.get(id)?;
                let value = inst.notional(p.qty(), *mark);
                Some(if inst.class == AssetClass::Perpetual {
                    value - p.cost_basis()
                } else {
                    value
                })
            })
            .sum();
        self.cash + held
    }
}

fn max_drawdown(initial: Notional, equity: &[Notional]) -> Decimal {
    let mut peak = initial;
    let mut worst = Decimal::ZERO;
    for &e in equity {
        if e > peak {
            peak = e;
        }
        if peak.is_positive() {
            worst = worst.max((peak - e) / peak);
        }
    }
    worst
}

/// Bars per 365-day year for a market that never closes.
fn bars_per_year(interval: Interval) -> Decimal {
    Decimal::from(365 * 86_400_000_i64) / Decimal::from(interval.as_millis())
}

fn sharpe(equity: &[Notional], interval: Interval) -> Option<Decimal> {
    let returns: Vec<Decimal> = equity
        .windows(2)
        .filter(|w| w[0].is_positive())
        .map(|w| w[1] / w[0] - Decimal::ONE)
        .collect();
    if returns.len() < 2 {
        return None;
    }
    let n = Decimal::from(returns.len());
    let mean = returns.iter().sum::<Decimal>() / n;
    let var = returns
        .iter()
        .map(|r| (*r - mean) * (*r - mean))
        .sum::<Decimal>()
        / (n - Decimal::ONE);
    let sd = var.sqrt()?;
    if sd.is_zero() {
        return None;
    }
    Some(mean / sd * bars_per_year(interval).sqrt()?)
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = |d: Decimal| format!("{}%", (d * Decimal::ONE_HUNDRED).round_dp(2));
        writeln!(
            f,
            "initial cash     {}",
            self.initial_cash.get().round_dp(8)
        )?;
        writeln!(
            f,
            "final equity     {}",
            self.final_equity.get().round_dp(8)
        )?;
        writeln!(
            f,
            "realized P&L     {}",
            self.realized_pnl.get().round_dp(8)
        )?;
        writeln!(f, "min cash         {}", self.min_cash.get().round_dp(8))?;
        writeln!(f, "max drawdown     {}", pct(self.max_drawdown))?;
        writeln!(f, "fills            {}", self.fills)?;
        writeln!(f, "closed trades    {}", self.closed_trades)?;
        writeln!(
            f,
            "win rate         {}",
            self.win_rate.map_or("n/a".into(), pct)
        )?;
        writeln!(
            f,
            "sharpe (ann.)    {}",
            self.sharpe
                .map_or("n/a".into(), |s| s.round_dp(4).to_string())
        )?;
        write!(f, "rejected signals {}", self.rejected_signals)?;
        if !self.funding.is_zero() {
            write!(f, "\nfunding          {}", self.funding.get().round_dp(8))?;
        }
        if let Some(e) = self.earn {
            write!(
                f,
                "\nearn interest    {} ({} of cash in Simple Earn at an assumed APR of {})",
                self.earn_interest.get().round_dp(8),
                pct(e.ratio),
                pct(e.apr)
            )?;
        }
        if let Some(why) = &self.killed {
            write!(
                f,
                "\nkill switch      tripped: {why}; the rest of the period wasn't traded"
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{HOUR, btcusdt, candle};
    use bot_core::{FillSpec, Liquidity, OrderId, Qty, Timestamp};
    use rust_decimal_macros::dec;

    fn fill(side: Side, qty: Decimal, price: Decimal, fee: Decimal, ts: i64) -> Fill {
        Fill::new(FillSpec {
            order_id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side,
            qty: Qty::new(qty),
            price: Price::new(price),
            fee: Fee::Quote(Notional::new(fee)),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(ts),
        })
        .unwrap()
    }

    /// Known-input regression test (CLAUDE.md DoD): fixed candles and fills,
    /// expected values worked by hand.
    ///
    /// Closes: c0 100, c1 110, c2 90, c3 120. Cash 1000.
    ///   buy  5 @ 100, fee 0.5 at c1 open -> cash 1000 - 500 - 0.5 = 499.5
    ///   sell 5 @ 120, fee 0.6 at c3 open -> cash 499.5 + 600 - 0.6 = 1098.9
    /// Equity at closes: 1000, 499.5 + 5*110 = 1049.5, 499.5 + 5*90 = 949.5,
    /// 1098.9.
    /// Realized = 599.4 - 500.5 = 98.9 = final - initial (cross-check).
    /// Max drawdown = (1049.5 - 949.5) / 1049.5 = 100 / 1049.5.
    /// Returns: 0.0495, -100/1049.5, 149.4/949.5; Sharpe is mean over
    /// sample sd, times sqrt(8760). Long-division values below were checked
    /// with Python's `decimal` module at 28 digits.
    #[test]
    fn report_on_known_input() {
        let id = btcusdt().id.clone();
        let candles = vec![
            candle(&id, 0, ["100", "100", "100", "100"]),
            candle(&id, 1, ["100", "110", "100", "110"]),
            candle(&id, 2, ["110", "110", "90", "90"]),
            candle(&id, 3, ["120", "120", "120", "120"]),
        ];
        let run = RunReport {
            events: 4,
            fills: vec![
                fill(Side::Buy, dec!(5), dec!(100), dec!(0.5), HOUR),
                fill(Side::Sell, dec!(5), dec!(120), dec!(0.6), 3 * HOUR),
            ],
            rejections: vec![],
            positions: vec![],
            ..Default::default()
        };
        let r = Report::compute(
            &run,
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
        );

        assert_eq!(r.final_equity, Notional::new(dec!(1098.9)));
        assert_eq!(r.realized_pnl, Notional::new(dec!(98.9)));
        // Cash 1000 -> 499.5 after the buy -> 1098.9 after the sell.
        assert_eq!(r.min_cash, Notional::new(dec!(499.5)));
        assert_eq!(r.max_drawdown, dec!(100) / dec!(1049.5));
        assert_eq!(r.max_drawdown.round_dp(10), dec!(0.0952834683));
        assert_eq!(r.fills, 2);
        assert_eq!(r.closed_trades, 1);
        assert_eq!(r.win_rate, Some(dec!(1)));
        assert_eq!(r.sharpe.unwrap().round_dp(6), SHARPE);
    }

    /// See `report_on_known_input`: mean 0.0371875010819..., sample sd
    /// 0.1267639802783..., times sqrt(8760) = 93.5947... -> 27.457006.
    const SHARPE: Decimal = dec!(27.457006);

    #[test]
    fn no_trades_no_ratios() {
        let id = btcusdt().id.clone();
        let candles = vec![
            candle(&id, 0, ["1", "1", "1", "1"]),
            candle(&id, 1, ["1", "1", "1", "1"]),
        ];
        let r = Report::compute(
            &RunReport::default(),
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
        );
        assert_eq!(r.final_equity, Notional::new(dec!(1000)));
        assert_eq!(r.min_cash, Notional::new(dec!(1000)));
        assert_eq!(r.max_drawdown, Decimal::ZERO);
        assert_eq!(r.win_rate, None);
        assert_eq!(r.sharpe, None);
    }

    /// Buying 20 @ 100 with 1000 cash spends 2000: cash -1000. The ledger
    /// doesn't trust the broker to have refused it, so it must expose it.
    #[test]
    fn overspend_shows_as_negative_min_cash() {
        let id = btcusdt().id.clone();
        let candles: Vec<Candle> = (0..2)
            .map(|i| candle(&id, i, ["100", "100", "100", "100"]))
            .collect();
        let run = RunReport {
            fills: vec![fill(Side::Buy, dec!(20), dec!(100), dec!(0), HOUR)],
            ..RunReport::default()
        };
        let r = Report::compute(
            &run,
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
        );
        assert_eq!(r.min_cash, Notional::new(dec!(-1000)));
    }

    /// Holding 10 @ 100 with cash 0, one open fills a buy of 5 @ 100 and a
    /// sell of the 10 @ 100 (a rotation into another coin, here one coin
    /// for brevity) in that order. Between them cash is -500, but they are
    /// one instant: cash goes 0 -> 500, never below 0.
    #[test]
    fn fills_of_one_instant_count_together() {
        let id = btcusdt().id.clone();
        let candles: Vec<Candle> = (0..3)
            .map(|i| candle(&id, i, ["100", "100", "100", "100"]))
            .collect();
        let run = RunReport {
            fills: vec![
                fill(Side::Buy, dec!(10), dec!(100), dec!(0), HOUR),
                fill(Side::Buy, dec!(5), dec!(100), dec!(0), 2 * HOUR),
                fill(Side::Sell, dec!(10), dec!(100), dec!(0), 2 * HOUR),
            ],
            ..RunReport::default()
        };
        let r = Report::compute(
            &run,
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
        );
        assert_eq!(r.min_cash, Notional::ZERO);
        assert_eq!(r.final_equity, Notional::new(dec!(1000)));
    }

    #[test]
    fn losing_sell_counts_against_win_rate() {
        let id = btcusdt().id.clone();
        let candles: Vec<Candle> = (0..4)
            .map(|i| candle(&id, i, ["100", "100", "100", "100"]))
            .collect();
        let run = RunReport {
            fills: vec![
                fill(Side::Buy, dec!(2), dec!(100), dec!(0), HOUR),
                fill(Side::Sell, dec!(1), dec!(110), dec!(0), 2 * HOUR),
                fill(Side::Sell, dec!(1), dec!(90), dec!(0), 3 * HOUR),
            ],
            ..RunReport::default()
        };
        let r = Report::compute(
            &run,
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
        );
        assert_eq!(r.closed_trades, 2);
        assert_eq!(r.win_rate, Some(dec!(0.5)));
        assert_eq!(r.realized_pnl, Notional::ZERO);
    }

    /// Cash 1000, no trades, 10% in Earn at an assumed 8.76% APR: each
    /// hour earns 1000 · 0.1 · 0.0876 / 8760 = 0.001. Three closes an hour
    /// apart: two hours, 0.002, in final equity and the equity curve.
    #[test]
    fn earn_interest_accrues_on_the_cash_share() {
        let id = btcusdt().id.clone();
        let candles: Vec<Candle> = (0..3)
            .map(|i| candle(&id, i, ["100", "100", "100", "100"]))
            .collect();
        let earn = EarnModel {
            ratio: dec!(0.1),
            apr: dec!(0.0876),
        };
        let r = Report::compute_with_earn(
            &RunReport::default(),
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
            Some(earn),
        );
        assert_eq!(r.earn_interest, Notional::new(dec!(0.002)));
        assert_eq!(r.final_equity, Notional::new(dec!(1000.002)));
        // Closes at hours 1, 2, 3 (−1 ms): 1000, then 0.001 more each hour.
        let curve: Vec<(i64, Decimal)> = r
            .equity_curve
            .iter()
            .map(|(t, e)| (t.as_millis(), e.get()))
            .collect();
        assert_eq!(
            curve,
            vec![
                (3_599_999, dec!(1000)),
                (7_199_999, dec!(1000.001)),
                (10_799_999, dec!(1000.002)),
            ]
        );
        assert!(r.to_string().contains("assumed APR of 8.76%"));
        let plain = Report::compute(
            &RunReport::default(),
            &candles,
            &[btcusdt()],
            Notional::new(dec!(1000)),
            Interval::H1,
        );
        assert_eq!(plain.final_equity, Notional::new(dec!(1000)));
        assert!(!plain.to_string().contains("earn"));
    }

    /// Perpetual short, wallet 10,000: sell 1 @ 30,000 (fee 15) at t1's
    /// open; funding 0.01% at mark 29,500 stamped 15 ms after t3's open,
    /// later on the clock than t3's buy back @ 29,000 (fee 14.5, stamped at
    /// the open) but settled before it, as the broker does (t3's fills
    /// come with t3's close): the short receives 2.95. Cash: 9,985 → 9,987.95 → +1,000 − 14.5 = 10,973.45.
    /// Equity per close: t0 10,000; t1 (31,000) 9,985 − 1,000 = 8,985;
    /// t2 (29,000) 9,985 + 1,000 = 10,985; t3 10,973.45 → max drawdown
    /// 1,015 / 10,000. Realized −15 + 2.95 + 985.5 = 973.45; one closed
    /// trade, a win. Funding after the buy would pay nothing (10,970.5).
    #[test]
    fn perpetuals_count_pnl_not_notional_and_book_funding_first() {
        let perp = Instrument::new(bot_core::InstrumentSpec {
            class: AssetClass::Perpetual,
            ..(*btcusdt()).clone()
        })
        .unwrap();
        let id = perp.id.clone();
        let candles = vec![
            candle(&id, 0, ["30000", "30000", "30000", "30000"]),
            candle(&id, 1, ["30000", "31000", "30000", "31000"]),
            candle(&id, 2, ["31000", "31000", "29000", "29000"]),
            candle(&id, 3, ["29000", "29000", "29000", "29000"]),
        ];
        let run = RunReport {
            fills: vec![
                fill(Side::Sell, dec!(1), dec!(30000), dec!(15), HOUR),
                fill(Side::Buy, dec!(1), dec!(29000), dec!(14.5), 3 * HOUR),
            ],
            ..Default::default()
        };
        let events = vec![MarketEvent::Funding {
            instrument: id.clone(),
            rate: dec!(0.0001),
            mark: Price::new(dec!(29500)),
            ts: Timestamp::from_millis(3 * HOUR + 15),
        }];
        let r = Report::compute_full(
            &run,
            &candles,
            &events,
            &[perp],
            Notional::new(dec!(10000)),
            Interval::H1,
            None,
        );
        assert_eq!(r.final_equity, Notional::new(dec!(10973.45)));
        assert_eq!(r.realized_pnl, Notional::new(dec!(973.45)));
        assert_eq!(r.funding, Notional::new(dec!(2.95)));
        assert_eq!(r.min_cash, Notional::new(dec!(9985)));
        assert_eq!(r.max_drawdown, dec!(0.1015));
        assert_eq!((r.closed_trades, r.win_rate), (1, Some(Decimal::ONE)));
    }
}
