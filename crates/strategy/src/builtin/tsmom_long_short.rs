//! `tsmom_long_short`: time-series momentum both ways, as Moskowitz, Ooi
//! and Pedersen (2012) tested it, for instruments that can be shorted.
//!
//! Each coin on its own, at its own close: its return over `lookback` bars
//! sets the side (up → long, down → short, unchanged → flat) and its
//! volatility the size: notional = `capital` / N × min(`max_weight`,
//! `target_vol` / annualized sd of the last `vol_period` returns), at the
//! deciding close, lot-rounded. Where the instrument can't be shorted
//! (spot), down means flat: long-only `tsmom` per coin.
//!
//! Trading (`builtin::signed`, shared): a flip closes first, reduce-only,
//! and opens the other side at the next decision, so a refused flip never
//! strands a losing position; exits and size cuts are reduce-only; a
//! same-side resize happens only beyond `band` (fraction of the target), so
//! vol drift doesn't trade every bar. Sizes come from `capital`, not the
//! wallet (invariant 5): keep `capital` below what the wallet can margin
//! after losses, or entries are refused. Dust counts as
//! flat; no decision on a coin while an order on it is working or
//! cancelling. Stateless. Leverage and margin are the broker's.
//!
//! Why long/short: the published evidence is for the long/short version,
//! and its best years were falls (2008, and in crypto 2022) where the short
//! side earned while long-only sat in cash. Costs: funding every settlement
//! on perpetuals (a short receives positive rates, a long pays), two taker
//! fees and slippages per flip, whipsaw in ranges, lag at turns, and a
//! liquidation price under every leveraged position. Edge unverified.

use crate::indicator;
use crate::{Context, Params, Strategy, StrategyError, StrategyRegistration, exact_decimal};
use bot_core::{Candle, Decimal, Instrument, InstrumentId, Notional, Signal};
use rust_decimal::MathematicalOps;
use serde::Deserialize;

const NAME: &str = "tsmom_long_short";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TsmomLongShortParams {
    /// Instruments traded (`["binance-usdm:BTCUSDT", "binance-usdm:ETHUSDT"]`);
    /// each gets `capital` / N.
    coins: Vec<InstrumentId>,
    /// Quote per run the sizes are fractions of (`"10000"`): notional, not
    /// margin.
    capital: Notional,
    /// Bars the return is measured over (`30`); at least 1.
    lookback: usize,
    /// Returns the volatility is measured over (`30`); at least 2.
    /// `history_len` must exceed max(lookback, vol_period).
    vol_period: usize,
    /// Annualized volatility each position is sized for (`"0.4"` = 40%).
    #[serde(deserialize_with = "exact_decimal")]
    target_vol: Decimal,
    /// Largest notional per coin as a multiple of its share (`"1"`); in
    /// (0, 3]. Above 1 needs leverage.
    #[serde(deserialize_with = "exact_decimal")]
    max_weight: Decimal,
    /// Resize only beyond this fraction of the target (`"0.2"`). In [0, 1).
    #[serde(deserialize_with = "exact_decimal")]
    band: Decimal,
    /// Fee per trade assumed; required, range-checked, unused in decisions.
    #[serde(deserialize_with = "exact_decimal")]
    fee: Decimal,
    /// Slippage per market trade assumed (bps); required, unused.
    #[serde(deserialize_with = "exact_decimal")]
    slippage_bps: Decimal,
}

/// See the module docs.
pub struct TsmomLongShort {
    p: TsmomLongShortParams,
}

impl TsmomLongShort {
    fn build(params: &Params) -> Result<Box<dyn Strategy>, StrategyError> {
        let p: TsmomLongShortParams = params.parse(NAME)?;
        let bad = |reason: &str| {
            Err(StrategyError::Params {
                strategy: NAME.to_owned(),
                reason: reason.to_owned(),
            })
        };
        if let Err(e) = super::portfolio::check_coins(&p.coins, 1) {
            return bad(&e);
        }
        if !p.capital.is_positive() {
            return bad("capital must be positive");
        }
        if p.lookback == 0 || p.vol_period < 2 {
            return bad("lookback must be >= 1 and vol_period >= 2");
        }
        if p.target_vol <= Decimal::ZERO {
            return bad("target_vol must be positive");
        }
        if p.max_weight <= Decimal::ZERO || p.max_weight > Decimal::from(3) {
            return bad("max_weight must be in (0, 3]");
        }
        if p.band < Decimal::ZERO || p.band >= Decimal::ONE {
            return bad("band must be in [0, 1)");
        }
        if p.fee < Decimal::ZERO || p.fee >= Decimal::ONE {
            return bad("fee must be in [0, 1)");
        }
        if p.slippage_bps < Decimal::ZERO || p.slippage_bps >= Decimal::from(10_000) {
            return bad("slippage_bps must be in [0, 10000)");
        }
        Ok(Box::new(Self { p }))
    }

    /// Signed lot-rounded units `coin` should hold at `c`'s close; `None`
    /// until there is enough history.
    fn target(&self, ctx: &Context, inst: &Instrument, c: &Candle) -> Option<Decimal> {
        let need = self.p.lookback.max(self.p.vol_period) + 1;
        let hist = ctx.history(&inst.id);
        if hist.len() < need {
            return None;
        }
        let then = hist.rev().nth(self.p.lookback)?.close.get();
        let now = c.close.get();
        let sign = match now.cmp(&then) {
            std::cmp::Ordering::Greater => Decimal::ONE,
            std::cmp::Ordering::Less if inst.allows_short() => -Decimal::ONE,
            _ => return Some(Decimal::ZERO),
        };
        let sd = indicator::return_sd(ctx.history(&inst.id), self.p.vol_period)?;
        let per_year = Decimal::from(365 * 86_400_000_i64) / Decimal::from(c.interval.as_millis());
        let vol = sd * per_year.sqrt()?;
        let weight = if vol.is_zero() {
            self.p.max_weight
        } else {
            (self.p.target_vol / vol).min(self.p.max_weight)
        };
        let notional = self.p.capital.get() * weight / Decimal::from(self.p.coins.len());
        let units = inst
            .qty_for(Notional::new(notional), c.close)
            .ok()
            .map(|q| inst.round_qty(q).get())?;
        Some(sign * units)
    }
}

inventory::submit! {
    StrategyRegistration { name: NAME, build: TsmomLongShort::build }
}

impl Strategy for TsmomLongShort {
    fn name(&self) -> &str {
        NAME
    }

    /// Decisions come from history and positions alone: only the format
    /// version is saved.
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
        let coin = &c.instrument;
        if !self.p.coins.contains(coin)
            || ctx.open_orders(coin).next().is_some()
            || ctx.cancelling_orders(coin).next().is_some()
        {
            return Vec::new();
        }
        let Some(inst) = ctx.instrument(coin) else {
            return Vec::new();
        };
        let Some(target) = self.target(ctx, inst, c) else {
            return Vec::new();
        };
        let held = super::signed::held(inst, ctx.position(coin).qty().get(), c);
        super::signed::step(inst, c, held, target, self.p.band)
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::test_support::{btcusdt, candle};
    use crate::{build, driver, names};
    use bot_core::{AssetClass, InstrumentSpec, OrderKind, Position, Qty, Side, Size};
    use rust_decimal_macros::dec;

    /// target_vol 10 (1000%) so `max_weight` binds and sizes are exact:
    /// one coin, capital 1000 → notional 1000.
    fn params(extra: &str) -> Params {
        Params::from_toml(&format!(
            r#"coins = ["binance:BTCUSDT"]
               capital = "1000"
               lookback = 2
               vol_period = 2
               target_vol = "10"
               max_weight = "1"
               band = "0.05"
               fee = "0.0005"
               slippage_bps = "2"
               {extra}"#
        ))
        .unwrap()
    }

    fn inst(perp: bool) -> Instrument {
        Instrument::new(InstrumentSpec {
            class: if perp {
                AssetClass::Perpetual
            } else {
                AssetClass::Spot
            },
            ..(*btcusdt()).clone()
        })
        .unwrap()
    }

    fn bar(s: &mut dyn Strategy, ctx: &mut Context, i: i64, price: &str) -> Vec<Signal> {
        let c = candle(i, price);
        driver::set_now(ctx, c.close_time);
        driver::push_candle(ctx, c.clone());
        s.on_candle(ctx, &c)
    }

    /// Signed holding at `price` (basis = |qty| · price, sign of qty).
    fn hold(ctx: &mut Context, qty: Decimal, price: Decimal) {
        driver::set_position(
            ctx,
            Position::from_parts(
                btcusdt().id.clone(),
                Qty::new(qty),
                Notional::new(qty * price),
                Notional::ZERO,
            )
            .unwrap(),
        );
    }

    fn submit(side: Side, q: Decimal) -> Vec<Signal> {
        vec![Signal::Submit {
            instrument: btcusdt().id.clone(),
            side,
            size: Size::Base(Qty::new(q)),
            kind: OrderKind::Market,
        }]
    }

    fn reduce(side: Side, q: Decimal) -> Vec<Signal> {
        vec![Signal::Reduce {
            instrument: btcusdt().id.clone(),
            side,
            qty: Qty::new(q),
            kind: OrderKind::Market,
        }]
    }

    #[test]
    fn self_registers_and_checks_params() {
        assert!(names().contains(&NAME));
        assert_eq!(build(NAME, &params("")).unwrap().name(), NAME);
        for bad in [
            ("lookback = 2", "lookback = 0"),
            ("vol_period = 2", "vol_period = 1"),
            ("max_weight = \"1\"", "max_weight = \"4\""),
            ("band = \"0.05\"", "band = \"1\""),
            ("target_vol = \"10\"", "target_vol = \"0\""),
        ] {
            let text = "coins = [\"binance:BTCUSDT\"]\ncapital = \"1000\"\nlookback = 2\nvol_period = 2\ntarget_vol = \"10\"\nmax_weight = \"1\"\nband = \"0.05\"\nfee = \"0.0005\"\nslippage_bps = \"2\"\n"
                .to_owned()
                .replace(bad.0, bad.1);
            assert!(
                build(NAME, &Params::from_toml(&text).unwrap()).is_err(),
                "{}",
                bad.1
            );
        }
    }

    /// Perpetual, closes 90, 95, 100: up 11% over 2 bars → long
    /// 1000 / 100 = 10. Then 98, 95: 95 < 100 two bars back → short: the
    /// flip first closes the 10, reduce-only. Next close 95 (< 98): from
    /// flat, short 1000 / 95 = 10.52631 (lot 1e-5). Then 95 vs 95: flat →
    /// reduce-only buy 10.52631.
    #[test]
    fn goes_long_flips_short_and_exits_reduce_only_on_a_perpetual() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = driver::new(vec![inst(true)], Params::default(), 10);
        assert!(bar(s.as_mut(), &mut ctx, 0, "90").is_empty());
        assert!(bar(s.as_mut(), &mut ctx, 1, "95").is_empty());
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 2, "100"),
            submit(Side::Buy, dec!(10))
        );
        hold(&mut ctx, dec!(10), dec!(100));
        assert!(
            bar(s.as_mut(), &mut ctx, 3, "98").is_empty(),
            "98 > 95: long"
        );
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 4, "95"),
            reduce(Side::Sell, dec!(10))
        );
        hold(&mut ctx, dec!(0), dec!(95));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 5, "95"),
            submit(Side::Sell, dec!(10.52631)),
            "95 < 98: short from flat"
        );
        hold(&mut ctx, dec!(-10.52631), dec!(95));
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 6, "95"),
            reduce(Side::Buy, dec!(10.52631)),
            "95 = 95 two bars back: flat"
        );
    }

    /// Spot can't short: down means flat, the long sold reduce-only.
    /// Same-side drift within the band (target 10.2 vs 10 held: 0.2 ≤
    /// 0.05 · 10.2) trades nothing.
    #[test]
    fn spot_goes_flat_and_small_drift_is_left_alone() {
        let mut s = build(NAME, &params("")).unwrap();
        let mut ctx = driver::new(vec![inst(false)], Params::default(), 10);
        bar(s.as_mut(), &mut ctx, 0, "90");
        bar(s.as_mut(), &mut ctx, 1, "95");
        hold(&mut ctx, dec!(10), dec!(100));
        // Target at 98.03: 1000 / 98.03 = 10.20095 → within the band.
        assert!(bar(s.as_mut(), &mut ctx, 2, "98.03").is_empty());
        bar(s.as_mut(), &mut ctx, 3, "98");
        assert_eq!(
            bar(s.as_mut(), &mut ctx, 4, "97"),
            reduce(Side::Sell, dec!(10)),
            "97 < 98.03: down, flat on spot"
        );
    }
}
