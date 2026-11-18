//! Data checks before a backtest is trusted.
//!
//! A cached series can be wrong in ways the replay can't see: eToro's
//! history is not split-adjusted and around some splits mixes both price
//! scales (NVDA 1,205 → 121 in one day), and a bad print looks like a
//! crash. The strategy then trades a move that never happened. Nothing
//! here is corrected or guessed: suspect bars are reported, and the user
//! decides (a real crash can move that much too).

use bot_core::{Candle, Decimal, InstrumentId, Price, Timestamp};

/// A bar whose price moved implausibly far: from the previous bar's close
/// to this bar's open or close, or between this bar's own open and close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuspectMove {
    pub instrument: InstrumentId,
    /// Open time of the bar.
    pub at: Timestamp,
    /// The price moved from …
    pub from: Price,
    /// … to this.
    pub to: Price,
}

impl SuspectMove {
    /// `to / from − 1`, the move as a fraction (`-0.9` = −90%).
    pub fn change(&self) -> Decimal {
        self.to.get() / self.from.get() - Decimal::ONE
    }
}

/// Every bar of `candles` (any instruments, any order) where a price moved
/// by more than `threshold` (`0.3` = up 30% or down to 1/1.3 of it, about
/// −23%: symmetric in ratio, so a halving and a doubling are both
/// flagged). Checked per instrument in time order: previous close → open,
/// previous close → close, open → close; one entry per bar, the largest
/// ratio. Oldest first per instrument, instruments in id order.
pub fn suspect_moves(candles: &[Candle], threshold: Decimal) -> Vec<SuspectMove> {
    let limit = Decimal::ONE + threshold;
    let mut sorted: Vec<&Candle> = candles.iter().collect();
    sorted.sort_by(|a, b| (&a.instrument, a.open_time).cmp(&(&b.instrument, b.open_time)));
    let ratio = |a: Price, b: Price| {
        let (a, b) = (a.get(), b.get());
        if a.is_zero() || b.is_zero() {
            Decimal::ONE
        } else if a > b {
            a / b
        } else {
            b / a
        }
    };
    let mut out = Vec::new();
    let mut prev: Option<&Candle> = None;
    for c in sorted {
        let before = prev
            .filter(|p| p.instrument == c.instrument)
            .map(|p| p.close);
        let mut pairs = vec![(c.open, c.close)];
        if let Some(b) = before {
            pairs.push((b, c.open));
            pairs.push((b, c.close));
        }
        if let Some((from, to)) = pairs
            .into_iter()
            .max_by_key(|(a, b)| ratio(*a, *b))
            .filter(|(a, b)| ratio(*a, *b) > limit)
        {
            out.push(SuspectMove {
                instrument: c.instrument.clone(),
                at: c.open_time,
                from,
                to,
            });
        }
        prev = Some(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{btcusdt, candle};
    use rust_decimal_macros::dec;

    /// Closes 100, 104, 10.4 (a 10-for-1 split), 10.5, then a bar opening
    /// 10.5 and closing 15 inside itself (+42.9%), then 15.1. Threshold
    /// 0.3: bar 2 (104 → 10.4, ratio 10) and bar 4 (open 10.5 → close 15,
    /// ratio 1.4286) are flagged; +4% and +1% moves aren't.
    #[test]
    fn flags_splits_and_wild_bars_only() {
        let id = btcusdt().id.clone();
        let bars = vec![
            candle(&id, 0, ["100", "100", "100", "100"]),
            candle(&id, 1, ["100", "104", "100", "104"]),
            candle(&id, 2, ["10.4", "10.4", "10.4", "10.4"]),
            candle(&id, 3, ["10.4", "10.5", "10.4", "10.5"]),
            candle(&id, 4, ["10.5", "15", "10.5", "15"]),
            candle(&id, 5, ["15", "15.1", "15", "15.1"]),
        ];
        let got = suspect_moves(&bars, dec!(0.3));
        let summary: Vec<(i64, Decimal, Decimal)> = got
            .iter()
            .map(|m| (m.at.as_millis() / 3_600_000, m.from.get(), m.to.get()))
            .collect();
        assert_eq!(
            summary,
            vec![(2, dec!(104), dec!(10.4)), (4, dec!(10.5), dec!(15))]
        );
        assert_eq!(got[0].change(), dec!(-0.9));
    }

    /// A fall to 1/1.3 is as suspect as a rise of 30%: 100 → 76 (ratio
    /// 1.3158) is flagged, 100 → 78 (1.2821) is not. Another instrument's
    /// bar in between doesn't count as "previous".
    #[test]
    fn falls_count_by_ratio_and_instruments_apart() {
        let btc = btcusdt().id.clone();
        let eth: InstrumentId = "binance:ETHUSDT".parse().unwrap();
        let bars = vec![
            candle(&btc, 0, ["100", "100", "100", "100"]),
            candle(&eth, 0, ["5", "5", "5", "5"]),
            candle(&btc, 1, ["76", "76", "76", "76"]),
            candle(&eth, 1, ["3.9", "3.9", "3.9", "3.9"]),
        ];
        let got = suspect_moves(&bars, dec!(0.3));
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].instrument.clone(), got[0].to.get()),
            (btc, dec!(76))
        );
    }
}
