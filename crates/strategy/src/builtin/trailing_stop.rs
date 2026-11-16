//! `trailing_stop`: buy-and-hold with a trailing exit and a re-entry.
//!
//! Buys `quote_amount` of one coin on its first candle. While long it
//! tracks the highest close since it got long (the peak); a close at or
//! below peak · (1 − `trail_pct`) sells the whole holding. While out it
//! tracks the lowest close since it got out (the trough); a close at or
//! above trough · (1 + `reentry_pct`) buys `quote_amount` again. All market
//! orders, decided on a close, filled at the next open.
//!
//! "Long" is what the books say: the lot-rounded holding passes the
//! venue's filters (`check_market`), so base-fee dust counts as out. The
//! peak or trough restarts from the current close whenever the books flip
//! (a fill landed), and from the current close again after the venue
//! refuses an order, so a refused buy (`-2010`) waits for a fresh rise
//! instead of being re-sent every bar.
//!
//! Why: it caps how far a holding can fall from its best close, the
//! simplest drawdown control there is. What it costs: in a choppy market it
//! sells near local lows and buys back higher, paying a round trip of fees
//! and slippage each time; a fast crash gaps past the stop (the fill is at
//! the next open, not at the stop level); `trail_pct` too tight whipsaws,
//! too loose gives back most of a rise. Compare against `buy_and_hold`:
//! the drawdown it saves must be worth the round trips. Expected edge is
//! unverified.
//!
//! State: the peak or trough, which side it was measured on, whether the
//! first buy was made, and the close time of the last bar folded in. Each
//! decision folds in every candle in the history newer than that, so bars
//! a restart or a reconnect delivered as backfill (no decision) still move
//! the peak. Reads only closed candles through `Context` (invariants 2, 5),
//! in `Decimal` (invariant 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size};
use serde::Deserialize;
use serde_json::json;

const NAME: &str = "trailing_stop";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrailingStopParams {
    /// The one instrument traded (`"binance:BTCUSDT"`).
    coin: InstrumentId,
    /// Quote spent on each entry (`"1000"` USDT), as a quote-budget market
    /// buy: never more than this.
    quote_amount: Notional,
    /// Fall from the peak close that sells (`"0.2"` = 20%). In (0, 1).
    #[serde(deserialize_with = "exact_decimal")]
    trail_pct: Decimal,
    /// Rise from the trough close that buys back (`"0.1"` = 10%). Positive.
    #[serde(deserialize_with = "exact_decimal")]
    reentry_pct: Decimal,
    /// Fee per trade assumed (`"0.001"`). Required so the config states the
    /// costs the backtest is run with; decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`).
    /// Required and unused, as `fee`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// The extreme close being tracked and on which side.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Track {
    long: bool,
    /// Peak when long, trough when out.
    extreme: Decimal,
    /// Close time (ms) of the newest candle folded in.
    last: i64,
}

/// See the module docs.
pub struct TrailingStop {
    p: TrailingStopParams,
    started: bool,
    track: Option<Track>,
}

impl TrailingStop {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: TrailingStopParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.trail_pct <= Decimal::ZERO || p.trail_pct >= Decimal::ONE {
            return bad("trail_pct must be in (0, 1)");
        }
        if p.reentry_pct <= Decimal::ZERO {
            return bad("reentry_pct must be positive");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self {
            p,
            started: false,
            track: None,
        }))
    }

    /// Lot-rounded units held, if a market sell of them passes the venue's
    /// filters at `c`'s close; `None` is out (flat or dust).
    fn long_units(&self, ctx: &Context, c: &Candle) -> Option<Qty> {
        let inst = ctx.instrument(&self.p.coin)?;
        let units = inst.round_qty(ctx.position(&self.p.coin).qty());
        inst.check_market(units, c.close, Side::Sell)
            .ok()
            .map(|()| units)
    }

    /// Brings the tracked extreme up to `c`: restarted from its close if
    /// the side changed (or nothing is tracked), else every candle newer
    /// than the last one folded moves it.
    fn fold(&mut self, ctx: &Context, c: &Candle, long: bool) -> Track {
        let now = c.close_time.as_millis();
        let track = match self.track {
            Some(t) if t.long == long => {
                let extreme = ctx
                    .history(&self.p.coin)
                    .filter(|h| h.close_time.as_millis() > t.last)
                    .map(|h| h.close.get())
                    .fold(t.extreme, |e, x| if long { e.max(x) } else { e.min(x) });
                Track {
                    long,
                    extreme,
                    last: now,
                }
            }
            _ => Track {
                long,
                extreme: c.close.get(),
                last: now,
            },
        };
        self.track = Some(track);
        track
    }

    fn buy(&self) -> Signal {
        Signal::Submit {
            instrument: self.p.coin.clone(),
            side: Side::Buy,
            size: Size::Quote(self.p.quote_amount),
            kind: OrderKind::Market,
        }
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: TrailingStop::build }
}

impl Strategy for TrailingStop {
    fn name(&self) -> &str {
        NAME
    }

    fn save_state(&self) -> Option<serde_json::Value> {
        Some(json!({
            "v": super::STATE_VERSION,
            "started": self.started,
            "track": self.track.map(|t| json!({
                "long": t.long,
                "extreme": t.extreme.to_string(),
                "last": t.last,
            })),
        }))
    }

    /// Takes the saved peak or trough as is. It can't contradict the books:
    /// if they flipped while the bot was down (a fill reconciled at
    /// startup), the next decision sees another side and restarts the
    /// track from the current close, as it would have live.
    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::check_version(NAME, &state)?;
        let err = |what: &str| super::state_error(NAME, what);
        let started = state["started"]
            .as_bool()
            .ok_or_else(|| err("bad started"))?;
        let track = match &state["track"] {
            serde_json::Value::Null => None,
            t => Some(Track {
                long: t["long"].as_bool().ok_or_else(|| err("bad long"))?,
                extreme: t["extreme"]
                    .as_str()
                    .and_then(|s| Decimal::from_str_exact(s).ok())
                    .filter(|e| e.is_sign_positive() && !e.is_zero())
                    .ok_or_else(|| err("bad extreme"))?,
                last: t["last"].as_i64().ok_or_else(|| err("bad last"))?,
            }),
        };
        self.started = started;
        self.track = track;
        Ok(())
    }

    /// Re-arm from the current close: a refused order is retried only on a
    /// fresh move, not every bar.
    fn on_rejected(&mut self, ctx: &mut Context, _s: &Signal, reason: &str) {
        tracing::warn!(
            strategy = NAME,
            reason,
            "order refused; waiting for a fresh move"
        );
        if let (Some(t), Some(c)) = (self.track.as_mut(), ctx.last_candle(&self.p.coin)) {
            t.extreme = c.close.get();
        }
    }

    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        if c.instrument != self.p.coin {
            return Vec::new();
        }
        let units = self.long_units(ctx, c);
        let track = self.fold(ctx, c, units.is_some());
        if ctx.open_orders(&self.p.coin).next().is_some()
            || ctx.cancelling_orders(&self.p.coin).next().is_some()
        {
            return Vec::new();
        }
        let close = c.close.get();
        if !self.started {
            self.started = true;
            return match units {
                None => vec![self.buy()],
                Some(_) => Vec::new(),
            };
        }
        match units {
            Some(q) if close <= track.extreme * (Decimal::ONE - self.p.trail_pct) => {
                vec![Signal::Submit {
                    instrument: self.p.coin.clone(),
                    side: Side::Sell,
                    size: Size::Base(q),
                    kind: OrderKind::Market,
                }]
            }
            None if close >= track.extreme * (Decimal::ONE + self.p.reentry_pct) => {
                vec![self.buy()]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::portfolio::tests::position;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::MarketEvent;
    use rust_decimal_macros::dec;

    fn params_with(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coin = "binance:BTCUSDT"
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn params() -> Params {
        params_with("trail_pct = \"0.1\"\nreentry_pct = \"0.05\"")
    }

    fn ctx() -> Context {
        driver::new(vec![btcusdt()], params(), 20)
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_event(ctx, &MarketEvent::Candle(c));
        out.extend(driver::take_signals(ctx));
        out
    }

    fn hold(ctx: &mut Context, qty: Decimal) {
        driver::set_position(ctx, position(&btcusdt().id, qty, qty * dec!(100), dec!(0)));
    }

    fn buy() -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
        }
    }

    fn sell(q: Decimal) -> Signal {
        Signal::Submit {
            instrument: btcusdt().id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params()).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        let bad = |extra: &str| build(NAME, &params_with(extra)).is_err();
        assert!(bad("trail_pct = \"0\"\nreentry_pct = \"0.05\""));
        assert!(bad("trail_pct = \"1\"\nreentry_pct = \"0.05\""));
        assert!(bad("trail_pct = \"0.1\"\nreentry_pct = \"0\""));
        assert!(bad("trail_pct = 0.1\nreentry_pct = \"0.05\""), "float");
        assert!(bad("trail_pct = \"0.1\""), "reentry_pct required");
    }

    /// Buys on the first candle. Long from 100: peak rises to 120; 108 =
    /// 120·0.9 exactly sells (at or below). Out from 107: trough 100;
    /// 105 = 100·1.05 buys back.
    #[test]
    fn exits_off_the_peak_and_reenters_off_the_trough() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        assert_eq!(feed(s.as_mut(), &mut ctx, 0, "100"), vec![buy()]);
        hold(&mut ctx, dec!(9.99));
        for (i, p) in [(1, "100"), (2, "120"), (3, "110"), (4, "108.01")] {
            assert!(feed(s.as_mut(), &mut ctx, i, p).is_empty(), "bar {i}");
        }
        assert_eq!(feed(s.as_mut(), &mut ctx, 5, "108"), vec![sell(dec!(9.99))]);
        hold(&mut ctx, dec!(0));
        for (i, p) in [(6, "107"), (7, "100"), (8, "104.99")] {
            assert!(feed(s.as_mut(), &mut ctx, i, p).is_empty(), "bar {i}");
        }
        assert_eq!(feed(s.as_mut(), &mut ctx, 9, "105"), vec![buy()]);
    }

    /// Dust from a base fee is out: it re-enters off the trough rather
    /// than trying to sell what can't be sold.
    #[test]
    fn dust_counts_as_out() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(0.00001));
        assert_eq!(feed(s.as_mut(), &mut ctx, 0, "100"), vec![buy()]);
        assert!(feed(s.as_mut(), &mut ctx, 1, "80").is_empty());
        assert_eq!(feed(s.as_mut(), &mut ctx, 2, "84"), vec![buy()]);
    }

    #[test]
    fn already_holding_at_start_trails_from_there() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(5));
        assert!(feed(s.as_mut(), &mut ctx, 0, "100").is_empty());
        assert_eq!(feed(s.as_mut(), &mut ctx, 1, "90"), vec![sell(dec!(5))]);
    }

    /// Bars that arrive as backfill (a restart's warm-up, a reconnect gap)
    /// are no decisions, but the next decision folds them into the peak.
    #[test]
    fn backfilled_bars_still_move_the_peak() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(5));
        feed(s.as_mut(), &mut ctx, 0, "100");
        let late = candle(1, "150");
        driver::push_candle(&mut ctx, late.clone());
        assert!(
            s.on_event(&mut ctx, &MarketEvent::Backfill(late))
                .is_empty()
        );
        // 134 ≤ 150·0.9 = 135: sells, though no decision saw 150.
        assert_eq!(feed(s.as_mut(), &mut ctx, 2, "134"), vec![sell(dec!(5))]);
    }

    #[test]
    fn a_refused_buy_waits_for_a_fresh_rise() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        feed(s.as_mut(), &mut ctx, 0, "100");
        feed(s.as_mut(), &mut ctx, 1, "90");
        assert_eq!(feed(s.as_mut(), &mut ctx, 2, "95"), vec![buy()]);
        s.on_rejected(&mut ctx, &buy(), "venue error -2010");
        assert!(feed(s.as_mut(), &mut ctx, 3, "95").is_empty());
        assert_eq!(feed(s.as_mut(), &mut ctx, 4, "99.75"), vec![buy()]);
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        let working = bot_core::Order::new(bot_core::OrderSpec {
            id: bot_core::OrderId::new("o-1").unwrap(),
            instrument: btcusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
            created_at: bot_core::Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        assert!(feed(s.as_mut(), &mut ctx, 0, "100").is_empty());
        driver::set_open_orders(&mut ctx, vec![]);
        assert_eq!(feed(s.as_mut(), &mut ctx, 1, "100"), vec![buy()]);
    }

    #[test]
    fn state_round_trips_and_bad_state_is_refused() {
        let mut s = build(NAME, &params()).unwrap();
        let mut ctx = ctx();
        hold(&mut ctx, dec!(5));
        feed(s.as_mut(), &mut ctx, 0, "100");
        feed(s.as_mut(), &mut ctx, 1, "120");
        let saved = s.save_state().unwrap();
        assert_eq!(
            saved,
            json!({"v": 1, "started": true,
                   "track": {"long": true, "extreme": "120", "last": 7_199_999}})
        );
        let mut fresh = build(NAME, &params()).unwrap();
        fresh.restore_state(saved, &ctx).unwrap();
        assert_eq!(
            feed(fresh.as_mut(), &mut ctx, 2, "108"),
            vec![sell(dec!(5))]
        );

        let mut s = build(NAME, &params()).unwrap();
        for bad in [
            json!({"v": 2, "started": true, "track": null}),
            json!({"v": 1, "track": null}),
            json!({"v": 1, "started": true, "track": {"long": true, "extreme": "-1", "last": 0}}),
            json!({"v": 1, "started": true, "track": {"long": true, "extreme": 120, "last": 0}}),
        ] {
            assert!(s.restore_state(bad.clone(), &ctx).is_err(), "{bad}");
        }
        assert!(
            s.restore_state(json!({"v": 1, "started": false, "track": null}), &ctx)
                .is_ok()
        );
    }
}
