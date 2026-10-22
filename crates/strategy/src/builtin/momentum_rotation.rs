//! `momentum_rotation`: hold whichever coin has the strongest recent
//! return, or cash when none is strong enough.
//!
//! Once per close time, after every coin's candle for it has arrived, each
//! coin's return over `lookback` bars is computed from closed candles:
//! `(close_now - close_lookback_ago) / close_lookback_ago`. The best coin is
//! the target if its return beats `min_momentum`; the strategy then holds
//! it and only it. If no coin beats `min_momentum`, the target is cash, and
//! everything is sold. That cash filter is the down-market defence
//! `ratio_hop` lacks: this strategy can be fully out of the market.
//!
//! It bets that relative winners keep winning over the next bars (time-
//! series and cross-sectional momentum). It fails on sharp reversals, where
//! yesterday's leader gaps down: it buys the leader at the next open, after
//! the move, and sells only once the lookback return has turned, a bar
//! late. Every switch pays a round trip (two fees, slippage on both market
//! legs), so choppy markets with frequent leadership changes bleed. Expected
//! edge is unverified.
//!
//! Each entry spends a fixed `quote_amount`, not the proceeds of the coin
//! being sold: sell and buy go out in the same evaluation. After a losing
//! hop the buy spends more than the sale raised. A backtest shows that as
//! negative `min_cash`, and live Binance refuses the buy (`-2010`), which
//! leaves the strategy in cash until an order is accepted.
//!
//! Decisions read only closed candles (invariant 2) through `Context`
//! (invariant 5) and return `Signal`s; arithmetic is `Decimal` (invariant
//! 1).

use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{
    Candle, Decimal, InstrumentId, Notional, OrderKind, Qty, Side, Signal, Size, Timestamp,
};
use serde::Deserialize;

const NAME: &str = "momentum_rotation";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MomentumRotationParams {
    /// Coins to choose between, all quoted in the same asset (the cash).
    /// One coin makes it a plain absolute-momentum filter.
    coins: Vec<InstrumentId>,
    /// Bars the return is measured over. Each coin needs `lookback + 1`
    /// closes before anything is decided, so the engine's `history_len`
    /// must be at least that.
    lookback: usize,
    /// Quote spent on each entry (`"1000"` USDT), as a quote-budget market
    /// buy.
    quote_amount: Notional,
    /// A coin must return more than this over `lookback` bars to be held
    /// (`"0.02"` = 2%). Default `"0"`: hold the best coin only while its
    /// return is positive, else cash.
    #[serde(deserialize_with = "exact_decimal", default)]
    min_momentum: Decimal,
    /// Fee per trade assumed (`"0.001"`). Required, like the other
    /// strategies', so the config states the costs the backtest runs with;
    /// decisions don't use it.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed, in basis points (`"5"`). Required
    /// and unused in decisions, as `fee`; match the backtest's
    /// `--slippage-bps`.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct MomentumRotation {
    p: MomentumRotationParams,
    last_eval: Option<Timestamp>,
    /// The universe doesn't fit (a coin missing, or two quote assets): never
    /// trade.
    stopped: bool,
}

impl MomentumRotation {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: MomentumRotationParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        let mut uniq = p.coins.clone();
        uniq.sort();
        uniq.dedup();
        if p.coins.is_empty() || uniq.len() != p.coins.len() {
            return bad("coins: need at least one, no duplicates");
        }
        if p.lookback == 0 {
            return bad("lookback must be >= 1");
        }
        if !p.quote_amount.is_positive() {
            return bad("quote_amount must be positive");
        }
        if p.min_momentum <= -Decimal::ONE {
            return bad("min_momentum must be > -1 (a return can't fall below -100%)");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self {
            p,
            last_eval: None,
            stopped: false,
        }))
    }

    /// Return over `lookback` bars, ending at the latest close; `None` until
    /// `lookback + 1` closes exist.
    fn momentum(&self, ctx: &Context, id: &InstrumentId) -> Option<Decimal> {
        let mut back = ctx.history(id).rev();
        let now = back.next()?.close.get();
        let then = back.nth(self.p.lookback - 1)?.close.get();
        Some((now - then) / then)
    }

    /// Lot-rounded units of `id` held, if a sell of them would pass the
    /// venue's filters at the last close. `None` is cash, including dust a
    /// buy's base fee left behind.
    fn held(ctx: &Context, id: &InstrumentId) -> Option<Qty> {
        let inst = ctx.instrument(id)?;
        let units = inst.round_qty(ctx.position(id).qty());
        let close = ctx.last_candle(id)?.close;
        inst.check_order(units, close).ok().map(|()| units)
    }

    fn evaluate(&self, ctx: &Context) -> Vec<Signal> {
        // Every coin needs a full lookback, or no coin is judged.
        let Some(returns) = self
            .p
            .coins
            .iter()
            .map(|id| self.momentum(ctx, id).map(|r| (id, r)))
            .collect::<Option<Vec<_>>>()
        else {
            return Vec::new();
        };
        // Strictly greater: a tie goes to the coin listed first.
        let mut best: Option<(&InstrumentId, Decimal)> = None;
        for (id, r) in returns {
            if best.is_none_or(|(_, b)| r > b) {
                best = Some((id, r));
            }
        }
        let target = best
            .filter(|(_, r)| *r > self.p.min_momentum)
            .map(|(id, _)| id);
        // Sells first, so live has the cash before the buy goes out.
        let mut out: Vec<Signal> = self
            .p
            .coins
            .iter()
            .filter(|id| Some(*id) != target)
            .filter_map(|id| {
                Self::held(ctx, id).map(|units| Signal::Submit {
                    instrument: id.clone(),
                    side: Side::Sell,
                    size: Size::Base(units),
                    kind: OrderKind::Market,
                })
            })
            .collect();
        if let Some(t) = target.filter(|t| Self::held(ctx, t).is_none()) {
            out.push(Signal::Submit {
                instrument: t.clone(),
                side: Side::Buy,
                size: Size::Quote(self.p.quote_amount),
                kind: OrderKind::Market,
            });
        }
        out
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: MomentumRotation::build }
}

impl Strategy for MomentumRotation {
    fn name(&self) -> &str {
        NAME
    }

    /// The last close time evaluated, so a restart doesn't evaluate it
    /// again. `stopped` isn't saved: `on_start` re-checks the universe.
    fn save_state(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "v": super::STATE_VERSION, "last_eval": self.last_eval }))
    }

    fn restore_state(
        &mut self,
        state: serde_json::Value,
        _ctx: &Context,
    ) -> Result<(), StrategyError> {
        super::check_version(NAME, &state)?;
        let last_eval: Option<Timestamp> = serde_json::from_value(state["last_eval"].clone())
            .map_err(|e| super::state_error(NAME, e))?;
        self.last_eval = last_eval;
        Ok(())
    }

    /// Checks the universe only; never trades.
    fn on_start(&mut self, ctx: &mut Context) {
        let quotes: Option<Vec<_>> = self
            .p
            .coins
            .iter()
            .map(|id| ctx.instrument(id).map(|i| i.quote.clone()))
            .collect();
        // Cash must be one asset, or "go to cash" means two things.
        self.stopped = !quotes.is_some_and(|q| q.windows(2).all(|w| w[0] == w[1]));
    }

    /// Evaluates once per close time, when every coin's candle for it has
    /// arrived, so no coin is judged on a stale price; and not while any of
    /// the coins has an order working, which deciding again could double.
    fn on_candle(&mut self, ctx: &mut Context, c: &Candle) -> Vec<Signal> {
        let t = c.close_time;
        if self.stopped || !self.p.coins.contains(&c.instrument) || self.last_eval == Some(t) {
            return Vec::new();
        }
        let complete = self
            .p
            .coins
            .iter()
            .all(|id| ctx.last_candle(id).is_some_and(|l| l.close_time == t));
        if !complete {
            return Vec::new();
        }
        self.last_eval = Some(t);
        if self
            .p
            .coins
            .iter()
            .any(|id| ctx.open_orders(id).next().is_some())
        {
            return Vec::new();
        }
        self.evaluate(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::btcusdt;
    use crate::{build, driver, names};
    use bot_core::{
        Asset, CandleSpec, Instrument, InstrumentSpec, Interval, Order, OrderId, OrderSpec,
        Position, Price,
    };
    use rust_decimal_macros::dec;

    const HOUR: i64 = 3_600_000;

    fn ethusdt() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance:ETHUSDT".parse().unwrap(),
            base: Asset::new("ETH").unwrap(),
            ..btcusdt().into()
        })
        .unwrap()
    }

    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT", "binance:ETHUSDT"]
               lookback = 2
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10"
               {extra}"#
        ))
        .unwrap()
    }

    fn start(extra: &str) -> (Box<dyn Strategy>, Context) {
        let p = params(extra);
        let mut ctx = driver::new(vec![btcusdt(), ethusdt()], p.clone(), 10);
        let mut s = build(NAME, &p).unwrap();
        s.on_start(&mut ctx);
        (s, ctx)
    }

    fn candle(i: &Instrument, n: i64, px: &str) -> Candle {
        let p: Price = px.parse().unwrap();
        Candle::new(CandleSpec {
            instrument: i.id.clone(),
            interval: Interval::H1,
            open_time: Timestamp::from_millis(n * HOUR),
            close_time: Timestamp::from_millis((n + 1) * HOUR - 1),
            open: p,
            high: p,
            low: p,
            close: p,
            volume: Qty::new(dec!(1)),
            quote_volume: Notional::new(p.get()),
        })
        .unwrap()
    }

    fn feed(s: &mut dyn Strategy, ctx: &mut Context, c: Candle) -> Vec<Signal> {
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        let mut out = s.on_candle(ctx, &c);
        out.extend(driver::take_signals(ctx));
        out
    }

    /// Bar `n` for both coins (BTC then ETH); returns all signals.
    fn bar(s: &mut dyn Strategy, ctx: &mut Context, n: i64, btc: &str, eth: &str) -> Vec<Signal> {
        let mut out = feed(s, ctx, candle(&btcusdt(), n, btc));
        out.extend(feed(s, ctx, candle(&ethusdt(), n, eth)));
        out
    }

    fn hold(ctx: &mut Context, i: &Instrument, qty: Decimal) {
        driver::set_position(
            ctx,
            Position::from_parts(
                i.id.clone(),
                Qty::new(qty),
                Notional::new(qty),
                Notional::ZERO,
            )
            .unwrap(),
        );
    }

    fn buy(i: &Instrument) -> Signal {
        Signal::Submit {
            instrument: i.id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
        }
    }

    fn sell(i: &Instrument, qty: Decimal) -> Signal {
        Signal::Submit {
            instrument: i.id.clone(),
            side: Side::Sell,
            size: Size::Base(Qty::new(qty)),
            kind: OrderKind::Market,
        }
    }

    #[test]
    fn self_registers() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
    }

    #[test]
    fn rejects_bad_params() {
        for bad in [
            r#"coins = []
               lookback = 2
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
            r#"coins = ["binance:BTCUSDT", "binance:BTCUSDT"]
               lookback = 2
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
            r#"coins = ["binance:BTCUSDT"]
               lookback = 0
               quote_amount = "1000"
               fee = "0.001"
               slippage_bps = "10""#,
            r#"coins = ["binance:BTCUSDT"]
               lookback = 2
               quote_amount = "1000"
               min_momentum = "-1"
               fee = "0.001"
               slippage_bps = "10""#,
            // A TOML number would pass through a float first (invariant 1).
            r#"coins = ["binance:BTCUSDT"]
               lookback = 2
               quote_amount = "1000"
               min_momentum = 0.01
               fee = "0.001"
               slippage_bps = "10""#,
            r#"coins = ["binance:BTCUSDT"]
               lookback = 2
               quote_amount = "1000"
               fee = "0.001""#,
        ] {
            assert!(
                build(NAME, &Params::from_toml(bad).unwrap()).is_err(),
                "{bad}"
            );
        }
    }

    /// lookback 2 needs 3 closes per coin: bars 0 and 1 decide nothing,
    /// however strong the move.
    #[test]
    fn insufficient_history_means_no_signal() {
        let (mut s, mut ctx) = start("");
        assert!(bar(s.as_mut(), &mut ctx, 0, "100", "10").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "200", "30").is_empty());
        // Bar 2 completes the window: BTC (300-100)/100 = 2 ties ETH
        // (30-10)/10 = 2, and a tie goes to the first-listed coin.
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "300", "30"),
            vec![buy(&btcusdt())]
        );
    }

    /// No decision until every coin's candle for the close time is in.
    #[test]
    fn waits_for_every_coin_at_the_close_time() {
        let (mut s, mut ctx) = start("");
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert!(feed(s.as_mut(), &mut ctx, candle(&btcusdt(), 2, "120")).is_empty());
        assert_eq!(
            feed(s.as_mut(), &mut ctx, candle(&ethusdt(), 2, "10")),
            vec![buy(&btcusdt())]
        );
    }

    /// BTC (110-100)/100 = 0.10 vs ETH (12-10)/10 = 0.20: ETH is the target.
    #[test]
    fn buys_the_strongest_coin_from_cash() {
        let (mut s, mut ctx) = start("");
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "110", "12"),
            vec![buy(&ethusdt())]
        );
    }

    /// Holding BTC while ETH leads: sell all the BTC, then buy ETH, in that
    /// order, in one evaluation.
    #[test]
    fn rotates_from_the_held_coin_to_the_leader() {
        let (mut s, mut ctx) = start("");
        hold(&mut ctx, &btcusdt(), dec!(9));
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "110", "12"),
            vec![sell(&btcusdt(), dec!(9)), buy(&ethusdt())]
        );
    }

    #[test]
    fn already_holding_the_leader_does_nothing() {
        let (mut s, mut ctx) = start("");
        hold(&mut ctx, &ethusdt(), dec!(80));
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert!(bar(s.as_mut(), &mut ctx, 2, "110", "12").is_empty());
    }

    /// Both coins down over the lookback: nothing beats min_momentum 0, so
    /// the holding is sold and nothing bought.
    #[test]
    fn goes_to_cash_when_no_coin_has_momentum() {
        let (mut s, mut ctx) = start("");
        hold(&mut ctx, &btcusdt(), dec!(9));
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "95", "9"),
            vec![sell(&btcusdt(), dec!(9))]
        );
    }

    /// ETH's 0.04 return beats BTC's 0.03 but not min_momentum 0.05: cash.
    #[test]
    fn min_momentum_is_the_cash_threshold() {
        let (mut s, mut ctx) = start(r#"min_momentum = "0.05""#);
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert!(bar(s.as_mut(), &mut ctx, 2, "103", "10.4").is_empty());
    }

    /// A restart holding both coins converges on the leader: the other one
    /// is sold, the leader kept.
    #[test]
    fn sells_every_coin_but_the_target() {
        let (mut s, mut ctx) = start("");
        hold(&mut ctx, &btcusdt(), dec!(9));
        hold(&mut ctx, &ethusdt(), dec!(80));
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "110", "12"),
            vec![sell(&btcusdt(), dec!(9))]
        );
    }

    /// 0.00000099 BTC rounds to zero lots: it is cash, not a holding to sell.
    #[test]
    fn dust_counts_as_cash() {
        let (mut s, mut ctx) = start("");
        hold(&mut ctx, &btcusdt(), dec!(0.00000099));
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert!(bar(s.as_mut(), &mut ctx, 2, "95", "9").is_empty());
    }

    #[test]
    fn waits_while_an_order_is_working() {
        let (mut s, mut ctx) = start("");
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        let working = Order::new(OrderSpec {
            id: OrderId::new("o-1").unwrap(),
            instrument: ethusdt().id.clone(),
            side: Side::Buy,
            size: Size::Quote(Notional::new(dec!(1000))),
            kind: OrderKind::Market,
            created_at: Timestamp::from_millis(0),
        })
        .unwrap();
        driver::set_open_orders(&mut ctx, vec![working]);
        assert!(bar(s.as_mut(), &mut ctx, 2, "110", "12").is_empty());
    }

    #[test]
    fn two_quote_assets_never_trade() {
        let p = params("");
        let ethbusd = Instrument::new(InstrumentSpec {
            id: "binance:ETHUSDT".parse().unwrap(),
            base: Asset::new("ETH").unwrap(),
            quote: Asset::new("BUSD").unwrap(),
            ..btcusdt().into()
        })
        .unwrap();
        let mut ctx = driver::new(vec![btcusdt(), ethbusd], p.clone(), 10);
        let mut s = build(NAME, &p).unwrap();
        s.on_start(&mut ctx);
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        assert!(bar(s.as_mut(), &mut ctx, 2, "110", "12").is_empty());
    }

    #[test]
    fn missing_coin_never_trades() {
        let p = params("");
        let mut ctx = driver::new(vec![btcusdt()], p.clone(), 10);
        let mut s = build(NAME, &p).unwrap();
        s.on_start(&mut ctx);
        for n in 0..3 {
            assert!(feed(s.as_mut(), &mut ctx, candle(&btcusdt(), n, "100")).is_empty());
        }
    }

    /// A close time already evaluated before the restart isn't evaluated
    /// again: the saved last_eval holds bar 2, so bar 2 decides nothing.
    #[test]
    fn restored_last_eval_prevents_a_second_decision() {
        let (mut s, mut ctx) = start("");
        bar(s.as_mut(), &mut ctx, 0, "100", "10");
        bar(s.as_mut(), &mut ctx, 1, "100", "10");
        let state = serde_json::json!({ "v": 1, "last_eval": (3 * HOUR - 1) });
        s.restore_state(state.clone(), &ctx).unwrap();
        assert!(bar(s.as_mut(), &mut ctx, 2, "110", "12").is_empty());
        assert_eq!(s.save_state(), Some(state));
        assert!(
            s.restore_state(serde_json::json!({ "v": 9 }), &ctx)
                .is_err()
        );
    }
}
