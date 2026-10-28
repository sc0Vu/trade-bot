//! `avwap`: a single-coin anchored-VWAP trend filter, long or flat.
//!
//! The anchored VWAP is the volume-weighted average price of every trade
//! since an anchor bar. A kline carries both the base volume and the quote
//! volume actually traded, so it is exact: the sum of quote volume over the
//! sum of base volume from the anchor through the current bar, not the
//! usual typical-price `(high + low + close) / 3` approximation. Holds the
//! coin while the close is above it (by more than `band_pct`), cash below:
//! above means the average buyer since the anchor is in profit.
//!
//! Anchors, all knowable when the bar closes (invariant 2):
//! - `day` / `week` / `month`: the first bar of the current UTC period
//!   (weeks start Monday);
//! - `swing_low` / `swing_high`: the bar with the lowest low / highest high
//!   of the last `swing_lookback` bars, current included (ties: the most
//!   recent).
//!
//! No decision while the anchor bar isn't in history (a period anchor needs
//! the period's first bar: a week of 15m bars is 672, so `history_len` must
//! cover it), while volume since the anchor is zero, or while an order on
//! the coin is working. Buy = quote-budget market buy (`quote_amount`), sell
//! = the whole holding; a holding too small to sell (fee dust) is cash.
//! Expected edge is unverified: a round trip per cross pays two fees and
//! slippage.

use super::calendar::{Period, period_start};
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, InstrumentId, Notional, OrderKind, Price, Qty, Side, Signal, Size,
};
use serde::Deserialize;

const NAME: &str = "avwap";

/// Where the average starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Anchor {
    Day,
    Week,
    Month,
    SwingLow,
    SwingHigh,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AvwapParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// `"day"`, `"week"`, `"month"`, `"swing_low"` or `"swing_high"`.
    /// Required: each anchors somewhere else.
    anchor: Anchor,
    /// Swing anchors only: bars searched for the swing, current included.
    swing_lookback: Option<usize>,
    /// Dead zone around the AVWAP (`"0.005"` = 0.5%): long only above
    /// `avwap * (1 + band_pct)`, cash only below `avwap * (1 - band_pct)`.
    #[serde(deserialize_with = "exact_decimal", default)]
    band_pct: Decimal,
    /// Quote spent each time the strategy goes long (`"100"`).
    quote_amount: Notional,
    /// Fee per trade assumed (`"0.001"`). Required and unused in decisions,
    /// as in the other single-coin built-ins.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`).
    /// Required and unused in decisions; match the backtest's
    /// `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct Avwap {
    p: AvwapParams,
}

impl Avwap {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: AvwapParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        let swing = matches!(p.anchor, Anchor::SwingLow | Anchor::SwingHigh);
        match (swing, p.swing_lookback) {
            (true, None | Some(0)) => return bad("swing anchors need swing_lookback >= 1"),
            (false, Some(_)) => return bad("swing_lookback applies to swing anchors only"),
            _ => {}
        }
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.band_pct < Decimal::ZERO || p.band_pct >= Decimal::ONE {
            return bad("band_pct must be in [0, 1)");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self { p }))
    }

    /// Index in `hist` of the anchor bar, if it is there.
    fn anchor_index(&self, hist: &[&Candle]) -> Option<usize> {
        let last = hist.last()?;
        match self.p.anchor {
            Anchor::Day | Anchor::Week | Anchor::Month => {
                let period = match self.p.anchor {
                    Anchor::Day => Period::Day,
                    Anchor::Week => Period::Week,
                    _ => Period::Month,
                };
                let start = period_start(period, last.open_time);
                hist.iter().position(|c| c.open_time == start)
            }
            Anchor::SwingLow | Anchor::SwingHigh => {
                let n = self.p.swing_lookback?;
                let from = hist.len().checked_sub(n)?;
                let low = self.p.anchor == Anchor::SwingLow;
                let mut best = from;
                for i in from + 1..hist.len() {
                    // `<=` / `>=`: on a tie the most recent bar wins.
                    let better = if low {
                        hist[i].low <= hist[best].low
                    } else {
                        hist[i].high >= hist[best].high
                    };
                    if better {
                        best = i;
                    }
                }
                Some(best)
            }
        }
    }

    /// Quote volume over base volume from the anchor bar through the
    /// current one; `None` without an anchor in history or with no volume.
    fn avwap(&self, ctx: &Context) -> Option<Price> {
        let inst = ctx.instrument(&self.p.coin)?;
        let hist: Vec<&Candle> = ctx.history(&self.p.coin).collect();
        let from = self.anchor_index(&hist)?;
        let (quote, base) = hist[from..]
            .iter()
            .fold((Notional::ZERO, Qty::ZERO), |(q, b), c| {
                (q + c.quote_volume, b + c.volume)
            });
        inst.price_for(quote, base).ok()
    }

    /// Lot-rounded units held, if a sell of them would pass the venue's
    /// filters at `c`'s close. `None` is cash: flat, or fee dust.
    fn long_units(&self, ctx: &Context, c: &Candle) -> Option<Qty> {
        let inst = ctx.instrument(&self.p.coin)?;
        let units = inst.round_qty(ctx.position(&self.p.coin).qty());
        inst.check_market(units, c.close, Side::Sell)
            .ok()
            .map(|()| units)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: Avwap::build }
}

impl Strategy for Avwap {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from history and positions alone, both rebuilt on a
    /// restart: only the format version is saved.
    fn save_state(&self) -> Option<serde_json::Value> {
        super::stateless_save()
    }

    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::stateless_restore(NAME, &state)
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let coin = &self.p.coin;
        if c.instrument != *coin || ctx.open_orders(coin).next().is_some() {
            return Vec::new();
        }
        let Some(avwap) = self.avwap(ctx) else {
            return Vec::new();
        };
        let (close, band) = (c.close.get(), self.p.band_pct);
        match self.long_units(ctx, c) {
            None if close > avwap.get() * (Decimal::ONE + band) => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            }],
            Some(units) if close < avwap.get() * (Decimal::ONE - band) => vec![Signal::Submit {
                instrument: coin.clone(),
                side: Side::Sell,
                size: Size::Base(units),
                kind: OrderKind::Market,
            }],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::btcusdt;
    use crate::{build, driver, names};
    use bot_core::Timestamp;
    use bot_core::{CandleSpec, Interval, Order, OrderId, OrderSpec, Position};
    use rust_decimal_macros::dec;

    const HOUR: i64 = 3_600_000;

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "100"
               fee = "0.001"
               slippage_bps = "5"
               {extra}"#
        ))
        .unwrap()
    }

    /// A 1h bar opening at `open_ms`: closes at `close`, ranges low..high,
    /// trades `v` BTC for `qv` USDT.
    fn bar(open_ms: i64, [low, high, close]: [&str; 3], v: &str, qv: &str) -> Candle {
        let p = |s: &str| s.parse::<Price>().unwrap();
        Candle::new(CandleSpec {
            instrument: btcusdt().id.clone(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(open_ms),
            close_time: Timestamp::from_millis(open_ms + HOUR - 1),
            open: p(close),
            high: p(high),
            low: p(low),
            close: p(close),
            volume: Qty::new(v.parse().unwrap()),
            quote_volume: Notional::new(qv.parse().unwrap()),
        })
        .unwrap()
    }

    fn flat(open_ms: i64, close: &str, v: &str, qv: &str) -> Candle {
        bar(open_ms, [close, close, close], v, qv)
    }

    /// Feeds `bars` as the engine does; returns the last bar's signals.
    fn feed(extra: &str, held: Decimal, bars: &[Candle]) -> Vec<Signal> {
        let p = params(extra);
        let mut ctx = driver::new(vec![btcusdt()], p.clone(), 50);
        driver::set_position(
            &mut ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(held),
                Notional::new(held * dec!(100)),
                Notional::ZERO,
            )
            .unwrap(),
        );
        let mut s = build(NAME, &p).unwrap();
        let mut out = Vec::new();
        for c in bars {
            driver::set_now(&mut ctx, c.close_time);
            driver::push_candle(&mut ctx, c.clone());
            out = s.on_candle(&mut ctx, c);
        }
        out
    }

    fn buy() -> Vec<Signal> {
        vec![Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(100))),
            kind: OrderKind::Market,
        }]
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
    }

    #[test]
    fn rejects_bad_params() {
        for extra in [
            r#"anchor = "swing_low""#,
            r#"anchor = "swing_high"
               swing_lookback = 0"#,
            r#"anchor = "day"
               swing_lookback = 5"#,
            r#"anchor = "hour""#,
            r#"anchor = "day"
               band_pct = "1""#,
            "",
        ] {
            assert!(build(NAME, &params(extra)).is_err(), "{extra:?}");
        }
        assert!(build(NAME, &params(r#"anchor = "week""#)).is_ok());
    }

    /// Day anchor, two bars of day 0: 2 BTC for 200 USDT, then 1 BTC for
    /// 130. AVWAP = 330 / 3 = 110 exactly. A close of 108 is below it (the
    /// plain mean of closes, 104, would say above): no buy. 111: buy.
    #[test]
    fn the_average_weights_by_volume_traded() {
        let first = flat(0, "100", "2", "200");
        assert!(
            feed(
                r#"anchor = "day""#,
                dec!(0),
                &[first.clone(), flat(HOUR, "108", "1", "130")]
            )
            .is_empty()
        );
        assert_eq!(
            feed(
                r#"anchor = "day""#,
                dec!(0),
                &[first, flat(HOUR, "111", "1", "130")]
            ),
            buy()
        );
    }

    /// Day 0 trades at 200 (hours 22, 23); day 1's first bar trades 1 BTC
    /// for 100 and closes 101. Anchored at day 1: AVWAP 100, buy. (With day
    /// 0 included it would be 500 / 3 = 166.67: no buy.)
    #[test]
    fn a_new_day_starts_a_new_average() {
        let bars = [
            flat(22 * HOUR, "200", "1", "200"),
            flat(23 * HOUR, "200", "1", "200"),
            flat(24 * HOUR, "101", "1", "100"),
        ];
        assert_eq!(feed(r#"anchor = "day""#, dec!(0), &bars), buy());
    }

    /// Holding 1 BTC; day 0 AVWAP (100 + 100 + 103) / 3 = 101, then a bar
    /// trading at 99 closes 99: (402) / 4 = 100.5 > 99, so sell all.
    #[test]
    fn below_the_average_it_sells_the_holding() {
        let bars = [
            flat(0, "100", "1", "100"),
            flat(HOUR, "100", "1", "100"),
            flat(2 * HOUR, "103", "1", "103"),
            flat(3 * HOUR, "99", "1", "99"),
        ];
        assert_eq!(
            feed(r#"anchor = "day""#, dec!(1), &bars),
            vec![Signal::Submit {
                instrument: btcusdt().id.clone(),
                side: Side::Sell,
                size: Size::Base(Qty::new(dec!(1))),
                kind: OrderKind::Market,
            }]
        );
    }

    /// Swing low over 3 bars: lows 95, 90, 92 anchor at the 90 bar. AVWAP
    /// over it and the current bar: (91 + 95) / 2 = 93 < close 94: buy.
    /// (Anchored at the first bar: (97 + 91 + 95) / 3 = 94.33, no buy.)
    #[test]
    fn a_swing_low_anchors_at_the_lowest_low() {
        let bars = [
            bar(0, ["95", "99", "97"], "1", "97"),
            bar(HOUR, ["90", "92", "91"], "1", "91"),
            bar(2 * HOUR, ["92", "96", "94"], "1", "95"),
        ];
        assert_eq!(
            feed(
                r#"anchor = "swing_low"
                   swing_lookback = 3"#,
                dec!(0),
                &bars
            ),
            buy()
        );
        // Two bars aren't three: no swing yet, no decision.
        assert!(
            feed(
                r#"anchor = "swing_low"
               swing_lookback = 3"#,
                dec!(0),
                &bars[1..]
            )
            .is_empty()
        );
    }

    /// Week anchor, but history starts on a Tuesday (1970-01-06): the
    /// week's first bar (Monday 00:00) isn't there, so no decision rather
    /// than a partial average. Zero volume since the anchor: none either.
    #[test]
    fn no_anchor_bar_or_no_volume_means_no_decision() {
        let tuesday = 5 * 24 * HOUR;
        assert!(
            feed(
                r#"anchor = "week""#,
                dec!(0),
                &[
                    flat(tuesday, "100", "1", "100"),
                    flat(tuesday + HOUR, "200", "1", "200")
                ]
            )
            .is_empty()
        );
        assert!(feed(r#"anchor = "day""#, dec!(0), &[flat(0, "100", "0", "0")]).is_empty());
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let p = params(r#"anchor = "day""#);
        let mut ctx = driver::new(vec![btcusdt()], p.clone(), 50);
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(100))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        let mut s = build(NAME, &p).unwrap();
        let c = flat(HOUR, "111", "1", "100");
        driver::push_candle(&mut ctx, c.clone());
        assert!(s.on_candle(&mut ctx, &c).is_empty());
    }
}
