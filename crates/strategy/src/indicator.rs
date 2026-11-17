//! Technical indicators over a strategy's closed-candle history.
//!
//! Each reads candles oldest first, ending at the current (closed) bar, as
//! [`Context::history`](crate::Context::history) yields them, so nothing
//! later than the current event can enter (invariant 2). Arithmetic is
//! `Decimal` throughout (invariant 1). Every indicator is a function of a
//! fixed window of the newest bars, never of where the history starts: a
//! backtest, a live run and a restart's warm-up see the same value on the
//! same bar as long as `history_len` covers the window.

use bot_core::{Candle, Decimal, Price};

/// True range of `c`: the largest of its high-low range and the distances
/// from `prev_close` (the bar before's close) to its high and its low. A gap
/// between bars counts as movement, which the high-low range alone would
/// miss.
pub fn true_range(c: &Candle, prev_close: Price) -> Decimal {
    let (high, low, prev) = (c.high.get(), c.low.get(), prev_close.get());
    (high - low)
        .max((high - prev).abs())
        .max((low - prev).abs())
}

/// Average true range: the mean of the last `period` true ranges, the
/// newest being the current bar's. A price distance in quote units, never
/// negative.
///
/// Needs `period + 1` candles (the oldest supplies only its close), so
/// `history_len` must be at least that; `None` with fewer, or for a `period`
/// of 0.
///
/// The simple-average form, not Wilder's smoothing
/// (`atr = (atr_prev·(n−1) + tr) / n`, seeded once). Wilder's value
/// depends on every bar since the seed, so it shifts with where the bounded
/// history happens to start and takes many periods to settle; the simple
/// average is exact from `period + 1` bars. It reacts faster to a volatility change and so differs
/// from charting tools that use Wilder's form.
pub fn atr<'a, I>(candles: I, period: usize) -> Option<Decimal>
where
    I: DoubleEndedIterator<Item = &'a Candle> + ExactSizeIterator,
{
    if period == 0 || candles.len() < period + 1 {
        return None;
    }
    let newest: Vec<&Candle> = candles.rev().take(period + 1).collect();
    // Newest first: each pair is (bar, the bar before it).
    let sum: Decimal = newest
        .windows(2)
        .map(|w| true_range(w[0], w[1].close))
        .sum();
    Some(sum / Decimal::from(period))
}

/// Per-bar volatility: the sample standard deviation of the last `period`
/// close-to-close simple returns (`close / previous close − 1`), the newest
/// ending at the current bar. A fraction per bar; annualize by multiplying
/// by √(bars per year).
///
/// Needs `period + 1` candles; `None` with fewer, or for a `period` under 2
/// (one return has no spread). Simple returns rather than log returns:
/// `Decimal` has no exact logarithm, and over one bar the two differ by
/// about half the squared return. The square root is `Decimal`'s
/// iterative approximation, so the result is close, not exact; use it to
/// size or filter, never as a money amount.
pub fn return_sd<'a, I>(candles: I, period: usize) -> Option<Decimal>
where
    I: DoubleEndedIterator<Item = &'a Candle> + ExactSizeIterator,
{
    use rust_decimal::MathematicalOps;
    if period < 2 || candles.len() < period + 1 {
        return None;
    }
    let closes: Vec<Decimal> = candles
        .rev()
        .take(period + 1)
        .map(|c| c.close.get())
        .collect();
    // Newest first: each pair is (close, the close before it).
    let returns: Vec<Decimal> = closes
        .windows(2)
        .map(|w| w[0] / w[1] - Decimal::ONE)
        .collect();
    let n = Decimal::from(period);
    let mean = returns.iter().sum::<Decimal>() / n;
    let var = returns
        .iter()
        .map(|r| (*r - mean) * (*r - mean))
        .sum::<Decimal>()
        / (n - Decimal::ONE);
    var.sqrt()
}

/// Exponential moving average of `values` (oldest first) with smoothing
/// α = 2 / (period + 1), seeded with the simple mean of the first `period`
/// values. Element `i` of the result is the EMA ending at
/// `values[period − 1 + i]`; empty if there are fewer than `period` values
/// or `period` is 0.
///
/// An EMA never forgets its seed, only discounts it by (1 − α) a step, so
/// callers pass a fixed window of the newest values rather than the whole
/// history: the result is then the same in a backtest, a live run and a
/// restart's warm-up, and differs from an EMA seeded long ago by at most
/// the seed's error times (1 − α)^(window − period).
pub fn ema_series(values: &[Decimal], period: usize) -> Vec<Decimal> {
    if period == 0 || values.len() < period {
        return Vec::new();
    }
    let n = Decimal::from(period);
    let alpha = Decimal::TWO / (n + Decimal::ONE);
    let mut ema = values[..period].iter().sum::<Decimal>() / n;
    let mut out = Vec::with_capacity(values.len() - period + 1);
    out.push(ema);
    for v in &values[period..] {
        ema += alpha * (*v - ema);
        out.push(ema);
    }
    out
}

/// Stochastic oscillator %K of the last bar of `bars` (oldest first):
/// where its close sits in the high-low range of the last `period` bars,
/// 0 at the lowest low, 100 at the highest high. A range of zero (no
/// movement) is 50. `None` with fewer than `period` bars or `period` 0.
pub fn stochastic_k(bars: &[&Candle], period: usize) -> Option<Decimal> {
    if period == 0 || bars.len() < period {
        return None;
    }
    let w = &bars[bars.len() - period..];
    let hi = w.iter().map(|c| c.high.get()).max()?;
    let lo = w.iter().map(|c| c.low.get()).min()?;
    let close = w.last()?.close.get();
    if hi == lo {
        return Some(Decimal::from(50));
    }
    Some(Decimal::from(100) * (close - lo) / (hi - lo))
}

/// (%K, %D) on the last bar of `bars` and on the one before it: %D is the
/// simple mean of the last `d_period` %K values. Needs `k_period +
/// d_period` bars (one more than the latest %D, for the previous pair);
/// `None` with fewer.
pub fn stochastic(
    bars: &[&Candle],
    k_period: usize,
    d_period: usize,
) -> Option<[(Decimal, Decimal); 2]> {
    if d_period == 0 || bars.len() < k_period + d_period {
        return None;
    }
    // %K for each of the last d_period + 1 bars, oldest first.
    let ks: Vec<Decimal> = (0..=d_period)
        .rev()
        .map(|back| stochastic_k(&bars[..bars.len() - back], k_period))
        .collect::<Option<_>>()?;
    let n = Decimal::from(d_period);
    let d = |w: &[Decimal]| w.iter().sum::<Decimal>() / n;
    let prev = (ks[d_period - 1], d(&ks[..d_period]));
    let now = (ks[d_period], d(&ks[1..]));
    Some([prev, now])
}

/// SuperTrend over `bars` (oldest first): direction on the last bar (`true`
/// up) and its line (the trailing lower band when up, upper when down).
///
/// Bands are `hl2 ± mult·ATR` (ATR = simple mean of `period` true ranges,
/// as [`atr`]), each ratcheting: the lower band never falls while the
/// trend is up and the close stays above it, the upper never rises while
/// down. The trend turns up when a close breaks above the upper band and
/// down when one breaks below the lower. The recursion starts at the first
/// bar with an ATR, trend up; callers pass a fixed window of the newest
/// bars, so the result is the same in a backtest, a live run and a
/// restart. Needs `period + 2` bars; `None` with fewer.
pub fn supertrend(bars: &[&Candle], period: usize, mult: Decimal) -> Option<(bool, Decimal)> {
    if period == 0 || bars.len() < period + 2 {
        return None;
    }
    let two = Decimal::TWO;
    let mut up = true;
    let (mut upper, mut lower) = (Decimal::ZERO, Decimal::ZERO);
    let mut prev_close = Decimal::ZERO;
    for end in period + 1..=bars.len() {
        let w = &bars[end - period - 1..end];
        let a = atr(w.iter().copied(), period)?;
        let c = w.last()?;
        let hl2 = (c.high.get() + c.low.get()) / two;
        let (basic_up, basic_low) = (hl2 + mult * a, hl2 - mult * a);
        if end == period + 1 {
            upper = basic_up;
            lower = basic_low;
        } else {
            if basic_up < upper || prev_close > upper {
                upper = basic_up;
            }
            if basic_low > lower || prev_close < lower {
                lower = basic_low;
            }
        }
        let close = c.close.get();
        if up && close < lower {
            up = false;
        } else if !up && close > upper {
            up = true;
        }
        prev_close = close;
    }
    Some((up, if up { lower } else { upper }))
}

/// Midpoint of the highest high and lowest low of `bars`.
fn midpoint(bars: &[&Candle]) -> Option<Decimal> {
    let hi = bars.iter().map(|c| c.high.get()).max()?;
    let lo = bars.iter().map(|c| c.low.get()).min()?;
    Some((hi + lo) / Decimal::TWO)
}

/// Ichimoku on the last bar of `bars` (oldest first): (tenkan, kijun,
/// cloud bottom, cloud top). Tenkan and kijun are the high-low midpoints of
/// the last `tenkan` and `kijun` bars; the cloud at a bar is the two spans
/// computed `displacement` bars earlier (span A = (tenkan + kijun) / 2,
/// span B = midpoint of `senkou_b` bars), as charts plot them shifted
/// forward: only past data. Needs `displacement + max(kijun, senkou_b)`
/// bars; `None` with fewer.
pub fn ichimoku(
    bars: &[&Candle],
    tenkan: usize,
    kijun: usize,
    senkou_b: usize,
    displacement: usize,
) -> Option<(Decimal, Decimal, Decimal, Decimal)> {
    let longest = tenkan.max(kijun).max(senkou_b);
    if tenkan == 0 || kijun == 0 || senkou_b == 0 || bars.len() < displacement + longest {
        return None;
    }
    let last = |n: usize, end: usize| &bars[end - n..end];
    let now = bars.len();
    let t = midpoint(last(tenkan, now))?;
    let k = midpoint(last(kijun, now))?;
    let then = now - displacement;
    let span_a = (midpoint(last(tenkan, then))? + midpoint(last(kijun, then))?) / Decimal::TWO;
    let span_b = midpoint(last(senkou_b, then))?;
    Some((t, k, span_a.min(span_b), span_a.max(span_b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{CandleSpec, Interval, Notional, Qty, Timestamp};
    use rust_decimal_macros::dec;

    fn bar(i: i64, o: &str, h: &str, l: &str, c: &str) -> Candle {
        let p = |s: &str| s.parse::<Price>().unwrap();
        Candle::new(CandleSpec {
            instrument: "binance:BTCUSDT".parse().unwrap(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(i * 3_600_000),
            close_time: Timestamp::from_millis(i * 3_600_000 + 3_599_999),
            open: p(o),
            high: p(h),
            low: p(l),
            close: p(c),
            volume: Qty::new(dec!(1)),
            quote_volume: Notional::new(p(c).get()),
        })
        .unwrap()
    }

    fn px(s: &str) -> Price {
        s.parse().unwrap()
    }

    /// Each of the three terms can be the largest.
    #[test]
    fn true_range_takes_the_widest_of_range_and_gaps() {
        // Prev close inside the bar 101..105: range 4, gaps 2 and 2.
        assert_eq!(
            true_range(&bar(1, "103", "105", "101", "104"), px("103")),
            dec!(4)
        );
        // Gap up: prev 95, bar 100..102: range 2, high-prev 7, low-prev 5.
        assert_eq!(
            true_range(&bar(1, "100", "102", "100", "101"), px("95")),
            dec!(7)
        );
        // Gap down: prev 110, bar 100..102: range 2, |102-110| 8, |100-110| 10.
        assert_eq!(
            true_range(&bar(1, "101", "102", "100", "101"), px("110")),
            dec!(10)
        );
        // No movement at all.
        assert_eq!(
            true_range(&bar(1, "100", "100", "100", "100"), px("100")),
            dec!(0)
        );
    }

    /// period 3 over 5 bars uses only the newest 4: bar 0's huge range never
    /// counts (only bar 1's close does, as bar 2's previous close).
    ///   bar 2: prev 100, 99..103   -> max(4, 3, 1)  = 4
    ///   bar 3: prev 102, 101..108  -> max(7, 6, 1)  = 7
    ///   bar 4: prev 107, 100..104  -> max(4, 3, 7)  = 7
    ///   ATR = (4 + 7 + 7) / 3 = 6
    #[test]
    fn averages_the_last_period_true_ranges() {
        let h = [
            bar(0, "100", "200", "50", "100"),
            bar(1, "100", "101", "99", "100"),
            bar(2, "100", "103", "99", "102"),
            bar(3, "102", "108", "101", "107"),
            bar(4, "104", "104", "100", "101"),
        ];
        assert_eq!(atr(h.iter(), 3), Some(dec!(6)));
        // period 4 reaches bar 1: prev 100, 99..101 -> max(2, 1, 1) = 2,
        // so (2 + 4 + 7 + 7) / 4 = 5. Bar 0 still only lends its close.
        assert_eq!(atr(h.iter(), 4), Some(dec!(5)));
    }

    /// Division stays exact to Decimal's precision: (1 + 1 + 2) / 3.
    #[test]
    fn a_non_terminating_average_is_decimal_not_float() {
        let h = [
            bar(0, "10", "10", "10", "10"),
            bar(1, "10", "11", "10", "11"),
            bar(2, "11", "12", "11", "12"),
            bar(3, "12", "14", "12", "14"),
        ];
        assert_eq!(atr(h.iter(), 3), Some(dec!(4) / dec!(3)));
    }

    #[test]
    fn needs_period_plus_one_candles() {
        let h = [
            bar(0, "100", "101", "99", "100"),
            bar(1, "100", "101", "99", "100"),
            bar(2, "100", "101", "99", "100"),
        ];
        assert_eq!(atr(h.iter(), 3), None, "3 candles give 2 true ranges");
        assert_eq!(atr(h.iter(), 2), Some(dec!(2)));
        assert_eq!(atr(h.iter(), 0), None);
        assert_eq!(atr([].iter(), 1), None);
    }

    fn closes(prices: &[&str]) -> Vec<Candle> {
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| bar(i as i64, p, p, p, p))
            .collect()
    }

    /// Returns +10%, −10%, 0: mean 0, sample variance (0.01 + 0.01 + 0) / 2
    /// = 0.01, sd 0.1. The 50 before them is outside a period of 3.
    #[test]
    fn return_sd_is_the_sample_sd_of_the_last_returns() {
        let h = closes(&["50", "100", "110", "99", "99"]);
        assert_eq!(return_sd(h.iter(), 3), Some(dec!(0.1)));
    }

    /// Steady growth has no spread; flat prices neither.
    #[test]
    fn return_sd_of_a_constant_return_is_zero() {
        let h = closes(&["100", "110", "121", "133.1"]);
        assert_eq!(return_sd(h.iter(), 3), Some(dec!(0)));
        let h = closes(&["100", "100", "100"]);
        assert_eq!(return_sd(h.iter(), 2), Some(dec!(0)));
    }

    #[test]
    fn return_sd_needs_period_plus_one_and_at_least_two_returns() {
        let h = closes(&["100", "110", "99"]);
        assert_eq!(return_sd(h.iter(), 3), None);
        assert_eq!(return_sd(h.iter(), 1), None, "one return has no spread");
        assert!(return_sd(h.iter(), 2).is_some());
    }

    /// 1..5, period 2: α = 2/3, seed mean(1, 2) = 1.5, then
    /// 1.5 + 2/3·(3 − 1.5) = 2.5, 3.5, 4.5.
    #[test]
    fn ema_series_seeds_with_the_mean_then_smooths() {
        let v = [dec!(1), dec!(2), dec!(3), dec!(4), dec!(5)];
        assert_eq!(
            ema_series(&v, 2),
            vec![dec!(1.5), dec!(2.5), dec!(3.5), dec!(4.5)]
        );
        // Period 1: α = 1, the values themselves.
        assert_eq!(ema_series(&v, 1), v.to_vec());
        assert!(ema_series(&v, 6).is_empty());
        assert!(ema_series(&v, 0).is_empty());
    }

    fn refs(c: &[Candle]) -> Vec<&Candle> {
        c.iter().collect()
    }

    /// Highs/lows/closes: (12, 8, 10), (14, 9, 13), (15, 11, 11). Over
    /// all 3: high 15, low 8, close 11 → %K = 100·3/7 = 42.857…
    #[test]
    fn stochastic_k_places_the_close_in_the_range() {
        let c = vec![
            bar(0, "10", "12", "8", "10"),
            bar(1, "10", "14", "9", "13"),
            bar(2, "13", "15", "11", "11"),
        ];
        let k = stochastic_k(&refs(&c), 3).unwrap();
        assert_eq!(k, dec!(300) / dec!(7));
        assert_eq!(stochastic_k(&refs(&c), 4), None);
        let flat = vec![bar(0, "5", "5", "5", "5")];
        assert_eq!(stochastic_k(&refs(&flat), 1), Some(dec!(50)));
    }

    /// k 2, d 2 over flat bars at 10, 12, 11, 14. %K over each bar and the
    /// one before: bar 1 (10, 12) → 100; bar 2 (12, 11) → 0; bar 3 (11, 14)
    /// → 100. prev = (%K 0, %D (100 + 0) / 2 = 50); now = (100, 50).
    #[test]
    fn stochastic_pairs_k_with_its_moving_average() {
        let c: Vec<Candle> = ["10", "12", "11", "14"]
            .iter()
            .enumerate()
            .map(|(i, p)| bar(i as i64, p, p, p, p))
            .collect();
        let [prev, now] = stochastic(&refs(&c), 2, 2).unwrap();
        assert_eq!(prev, (dec!(0), dec!(50)));
        assert_eq!(now, (dec!(100), dec!(50)));
        assert_eq!(stochastic(&refs(&c[..3]), 2, 2), None);
    }

    /// A steady climb (close 100 + 2i, high +1, low −2) stays up with the
    /// line on the lower band, 107.5; a crash to 90 turns it down onto the
    /// upper band, 119.5. Both from a separate Python run of the rule.
    #[test]
    fn supertrend_follows_and_flips() {
        let climb: Vec<Candle> = (0..8)
            .map(|i| {
                let c = Decimal::from(100 + 2 * i);
                let (o, h, l) = (c - dec!(1), c + dec!(1), c - dec!(2));
                bar(
                    i,
                    &o.to_string(),
                    &h.to_string(),
                    &l.to_string(),
                    &c.to_string(),
                )
            })
            .collect();
        assert_eq!(
            supertrend(&refs(&climb), 3, dec!(2)),
            Some((true, dec!(107.5)))
        );
        let mut crash = climb.clone();
        crash.push(bar(8, "114", "114", "90", "90"));
        assert_eq!(
            supertrend(&refs(&crash), 3, dec!(2)),
            Some((false, dec!(119.5)))
        );
        assert_eq!(supertrend(&refs(&climb[..4]), 3, dec!(2)), None);
    }

    /// tenkan 2, kijun 3, senkou_b 4, displacement 2 over 6 flat bars at
    /// 10, 11, 12, 13, 14, 15. Now: tenkan mid(14,15) = 14.5, kijun
    /// mid(13..15) = 14. Two bars back (end at 13): tenkan mid(12,13) =
    /// 12.5, kijun mid(11..13) = 12 → span A 12.25; span B mid(10..13) =
    /// 11.5. Cloud 11.5..12.25.
    #[test]
    fn ichimoku_cloud_comes_from_displaced_spans() {
        let c: Vec<Candle> = (0..6)
            .map(|i| {
                let p = (10 + i).to_string();
                bar(i, &p, &p, &p, &p)
            })
            .collect();
        assert_eq!(
            ichimoku(&refs(&c), 2, 3, 4, 2),
            Some((dec!(14.5), dec!(14), dec!(11.5), dec!(12.25)))
        );
        assert_eq!(ichimoku(&refs(&c[..5]), 2, 3, 4, 2), None);
    }
}
