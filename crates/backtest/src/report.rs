//! Backtest report: P&L, drawdown, trade stats and Sharpe, all `Decimal`
//! (invariant 1).
//!
//! The paper broker keeps no cash balance, so the report keeps its own
//! ledger: it replays the fills, in order, against the candle stream.
//! Equity (cash plus holdings marked at each instrument's latest close) is
//! taken once per close time, after every candle and fill up to it.

use bot_core::{
    Candle, Decimal, Fee, Fill, Instrument, InstrumentId, Interval, Notional, Position, Price, Side,
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
}

impl Report {
    /// Builds the report from a finished run and the candles it replayed.
    pub fn compute(
        run: &RunReport,
        candles: &[Candle],
        instruments: &[Instrument],
        initial_cash: Notional,
        interval: Interval,
    ) -> Self {
        let by_id: HashMap<&InstrumentId, &Instrument> =
            instruments.iter().map(|i| (&i.id, i)).collect();
        let mut sorted: Vec<&Candle> = candles.iter().collect();
        sorted.sort_by(|a, b| (a.close_time, &a.instrument).cmp(&(b.close_time, &b.instrument)));

        let mut ledger = Ledger {
            cash: initial_cash,
            positions: HashMap::new(),
            marks: HashMap::new(),
            closed: 0,
            wins: 0,
        };
        let mut fills = run.fills.iter().peekable();
        let mut equity = Vec::new();
        let mut i = 0;
        while i < sorted.len() {
            let t = sorted[i].close_time;
            while let Some(f) = fills.next_if(|f| f.ts <= t) {
                if let Some(inst) = by_id.get(&f.instrument) {
                    ledger.apply(f, inst);
                }
            }
            while i < sorted.len() && sorted[i].close_time == t {
                ledger
                    .marks
                    .insert(sorted[i].instrument.clone(), sorted[i].close);
                i += 1;
            }
            equity.push(ledger.equity(&by_id));
        }
        // Fills after the last candle can't happen (fills need a candle),
        // but apply any so the ledger is complete.
        for f in fills {
            if let Some(inst) = by_id.get(&f.instrument) {
                ledger.apply(f, inst);
            }
        }

        let realized_pnl = ledger.positions.values().map(Position::realized_pnl).sum();
        let final_equity = ledger.equity(&by_id);
        let win_rate =
            (ledger.closed > 0).then(|| Decimal::from(ledger.wins) / Decimal::from(ledger.closed));
        Self {
            initial_cash,
            final_equity,
            realized_pnl,
            max_drawdown: max_drawdown(initial_cash, &equity),
            fills: run.fills.len(),
            closed_trades: ledger.closed,
            win_rate,
            sharpe: sharpe(&equity, interval),
            rejected_signals: run.rejections.len(),
        }
    }
}

struct Ledger {
    cash: Notional,
    positions: HashMap<InstrumentId, Position>,
    marks: HashMap<InstrumentId, Price>,
    closed: usize,
    wins: usize,
}

impl Ledger {
    fn apply(&mut self, f: &Fill, inst: &Instrument) {
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

    fn equity(&self, by_id: &HashMap<&InstrumentId, &Instrument>) -> Notional {
        let held: Notional = self
            .positions
            .iter()
            .filter_map(|(id, p)| {
                let mark = self.marks.get(id)?;
                Some(by_id.get(id)?.notional(p.qty(), *mark))
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
        write!(f, "rejected signals {}", self.rejected_signals)
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
        assert_eq!(r.max_drawdown, Decimal::ZERO);
        assert_eq!(r.win_rate, None);
        assert_eq!(r.sharpe, None);
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
}
