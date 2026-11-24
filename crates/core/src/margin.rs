//! Margin for linear perpetuals (Binance USDⓈ-M, one-way mode), and the
//! per-position close-out of CFDs (eToro, [`close_out_price`]).
//!
//! **Maintenance margin** comes from a notional bracket (Binance's
//! "Notional and Leverage Brackets", `GET /fapi/v1/leverageBracket`):
//! `notional × rate − cum`, where `cum` ("maintenance amount") makes the
//! piecewise-linear schedule continuous. Larger positions sit in brackets
//! with higher rates and lower maximum leverage. Notional is valued at the
//! mark price.
//!
//! **Initial margin** is the notional divided by the leverage chosen.
//!
//! **Liquidation price** (one-way mode), as Binance's FAQ "How to Calculate
//! Liquidation Price of USDⓈ-M Contracts" gives it:
//!
//! `LP = (WB − TMM₁ + UPNL₁ + cum − side·size·EP) / (size·MMR − side·size)`
//!
//! with WB the wallet balance (cross: the cross wallet; isolated: the
//! position's isolated wallet, and TMM₁ = UPNL₁ = 0), TMM₁ and UPNL₁ the
//! maintenance margin and unrealized P&L of the other cross positions,
//! side +1 long / −1 short, size the absolute quantity, EP the entry
//! price, MMR and cum from the bracket the position's notional at LP falls
//! in (if it falls in another, the FAQ says recompute with that one).
//! Pinned by the FAQ's own worked example (tests).

use crate::{Error, Notional, Position, Price};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// One notional bracket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bracket {
    /// Lowest notional in the bracket (inclusive).
    pub floor: Notional,
    /// Highest notional (exclusive), the next bracket's floor.
    pub cap: Notional,
    /// Most leverage a position this size may use.
    pub max_leverage: u32,
    /// Maintenance margin rate (`0.004` = 0.4%).
    #[serde(with = "crate::instrument::decimal_str")]
    pub rate: Decimal,
    /// Maintenance amount ("cum"), in the quote.
    #[serde(with = "crate::instrument::decimal_str")]
    pub cum: Decimal,
}

/// An instrument's brackets, contiguous from notional 0 upward.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Bracket>", into = "Vec<Bracket>")]
pub struct Brackets(Vec<Bracket>);

impl TryFrom<Vec<Bracket>> for Brackets {
    type Error = Error;
    fn try_from(v: Vec<Bracket>) -> Result<Self, Error> {
        Self::new(v)
    }
}

impl From<Brackets> for Vec<Bracket> {
    fn from(b: Brackets) -> Self {
        b.0
    }
}

impl Brackets {
    /// Checks the schedule: non-empty, starting at 0, each bracket's cap
    /// the next one's floor, leverage ≥ 1, rate in [0, 1), cum ≥ 0.
    pub fn new(mut v: Vec<Bracket>) -> Result<Self, Error> {
        let bad = |why| Err(Error::InvalidInstrument(why));
        v.sort_by_key(|b| b.floor);
        let Some(first) = v.first() else {
            return bad("no margin brackets");
        };
        if !first.floor.is_zero() {
            return bad("margin brackets must start at notional 0");
        }
        for w in v.windows(2) {
            if w[0].cap != w[1].floor {
                return bad("margin brackets must be contiguous");
            }
        }
        for b in &v {
            if b.cap <= b.floor
                || b.max_leverage == 0
                || b.rate < Decimal::ZERO
                || b.rate >= Decimal::ONE
                || b.cum < Decimal::ZERO
            {
                return bad("bad margin bracket");
            }
        }
        Ok(Self(v))
    }

    /// The bracket a position of `notional` (absolute) falls in; past the
    /// last cap, the last.
    pub fn for_notional(&self, notional: Notional) -> &Bracket {
        let n = notional.abs();
        self.0
            .iter()
            .find(|b| n >= b.floor && n < b.cap)
            .unwrap_or_else(|| self.0.last().unwrap_or_else(|| unreachable!("non-empty")))
    }

    /// Maintenance margin of a position of `notional`: notional × rate −
    /// cum of its bracket.
    pub fn maintenance(&self, notional: Notional) -> Notional {
        let b = self.for_notional(notional);
        Notional::new(notional.get().abs() * b.rate - b.cum)
    }

    /// Most leverage a position of `notional` may use.
    pub fn max_leverage(&self, notional: Notional) -> u32 {
        self.for_notional(notional).max_leverage
    }

    pub fn iter(&self) -> impl Iterator<Item = &Bracket> {
        self.0.iter()
    }
}

/// How a position's margin is held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarginMode {
    /// Each position has its own wallet; only it can be lost.
    #[default]
    Isolated,
    /// All positions share the cross wallet.
    Cross,
}

/// The share of a CFD position's margin whose loss closes it: eToro's
/// terms (UK, 7.4) keep a position's margin at no less than 50% of what
/// it might lose, the ESMA/FCA retail "margin close-out rule", applied per
/// position, whatever else the account holds (7.5).
pub const CFD_CLOSE_OUT: Decimal = Decimal::from_parts(5, 0, 0, false, 1);

/// The price at which a CFD position opened at `entry` with `leverage`
/// is force-closed: where its loss reaches `ratio` of its margin. Margin
/// is |qty| × entry / leverage, so the loss |qty| × |move| reaches
/// ratio × margin at a move of entry × ratio / leverage, independent of
/// size: long `entry × (1 − ratio/leverage)`, short `entry × (1 +
/// ratio/leverage)`. Not rounded to tick (a trigger, not an order price).
/// `None` when flat, or for a long whose close-out would be at or below
/// zero (ratio ≥ leverage: it can't lose that share of its margin).
pub fn close_out_price(position: &Position, leverage: u32, ratio: Decimal) -> Option<Price> {
    let qty = position.qty().get();
    if qty.is_zero() {
        return None;
    }
    let entry = position.cost_basis().get() / qty;
    let step = ratio / Decimal::from(leverage.max(1));
    let price = if qty.is_sign_positive() {
        entry * (Decimal::ONE - step)
    } else {
        entry * (Decimal::ONE + step)
    };
    (price > Decimal::ZERO).then(|| Price::new(price))
}

/// Margin to open `notional` at `leverage` (≥ 1): notional / leverage.
pub fn initial_margin(notional: Notional, leverage: u32) -> Notional {
    Notional::new(notional.get().abs() / Decimal::from(leverage.max(1)))
}

/// Unrealized P&L of `position` at `mark`: qty × mark − entry notional
/// (signed: a short gains as the mark falls).
pub fn unrealized_pnl(position: &Position, mark: Price) -> Notional {
    Notional::new(position.qty().get() * mark.get() - position.cost_basis().get())
}

/// The mark at which the venue liquidates `position` (module docs).
/// `wallet` is the isolated wallet (isolated) or the cross wallet balance
/// (cross); `others_maintenance` / `others_upnl` are the other cross
/// positions' (zero for isolated). `None` when flat, or when no mark above
/// zero liquidates it (a long margined beyond its notional).
pub fn liquidation_price(
    position: &Position,
    wallet: Notional,
    others_maintenance: Notional,
    others_upnl: Notional,
    brackets: &Brackets,
) -> Option<Price> {
    let qty = position.qty().get();
    if qty.is_zero() {
        return None;
    }
    let size = qty.abs();
    let side = if qty.is_sign_negative() {
        -Decimal::ONE
    } else {
        Decimal::ONE
    };
    let entry = position.cost_basis().get().abs() / size;
    let base = wallet.get() - others_maintenance.get() + others_upnl.get() - side * size * entry;
    // The bracket that applies is the one the notional at LP falls in:
    // try each, keep the one that is self-consistent.
    let mut fallback = None;
    for b in brackets.iter() {
        let denom = size * b.rate - side * size;
        if denom.is_zero() {
            continue;
        }
        let lp = (base + b.cum) / denom;
        if lp <= Decimal::ZERO {
            continue;
        }
        let at = Notional::new(size * lp);
        if brackets.for_notional(at) == b {
            return Some(Price::new(lp));
        }
        fallback.get_or_insert(lp);
    }
    fallback.map(Price::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstrumentId, Qty};
    use rust_decimal_macros::dec;

    fn one(rate: Decimal, cum: Decimal) -> Brackets {
        Brackets::new(vec![Bracket {
            floor: Notional::ZERO,
            cap: Notional::new(dec!(1000000000)),
            max_leverage: 20,
            rate,
            cum,
        }])
        .unwrap()
    }

    fn pos(symbol: &str, qty: Decimal, entry: Decimal) -> Position {
        Position::from_parts(
            format!("binance:{symbol}").parse::<InstrumentId>().unwrap(),
            Qty::new(qty),
            Notional::new(qty * entry),
            Notional::ZERO,
        )
        .unwrap()
    }

    /// Binance's FAQ example (one-way, cross): wallet 1,535,443.01; ETH
    /// 3,683.979 @ 1,456.84 (mark 1,335.18, MMR 10%, cum 135,365); BTC
    /// 109.488 @ 32,481.98 (mark 31,967.27, MMR 2.5%, cum 16,300). Each
    /// one's maintenance margin is the other's TMM₁, its unrealized P&L
    /// the other's UPNL₁. The FAQ's answers: maintenance 356,512.508 and
    /// 71,200.81144; liquidation 1,153.26 and 26,316.89.
    #[test]
    fn matches_binances_worked_example() {
        let eth = pos("ETHUSDT", dec!(3683.979), dec!(1456.84));
        let btc = pos("BTCUSDT", dec!(109.488), dec!(32481.98));
        let (eth_b, btc_b) = (one(dec!(0.10), dec!(135365)), one(dec!(0.025), dec!(16300)));
        let (eth_mark, btc_mark) = (Price::new(dec!(1335.18)), Price::new(dec!(31967.27)));
        let notional = |p: &Position, m: Price| Notional::new(p.qty().get() * m.get());
        let eth_mm = eth_b.maintenance(notional(&eth, eth_mark));
        let btc_mm = btc_b.maintenance(notional(&btc, btc_mark));
        assert_eq!(eth_mm.get().round_dp(3), dec!(356512.508));
        assert_eq!(btc_mm.get().round_dp(5), dec!(71200.81144));
        // UPNL per the FAQ's table: −448,192.89 and −56,354.57.
        assert_eq!(
            unrealized_pnl(&eth, eth_mark).get().round_dp(2),
            dec!(-448192.89)
        );
        assert_eq!(
            unrealized_pnl(&btc, btc_mark).get().round_dp(2),
            dec!(-56354.57)
        );
        let wb = Notional::new(dec!(1535443.01));
        let eth_lp = liquidation_price(&eth, wb, btc_mm, unrealized_pnl(&btc, btc_mark), &eth_b);
        let btc_lp = liquidation_price(&btc, wb, eth_mm, unrealized_pnl(&eth, eth_mark), &btc_b);
        assert_eq!(eth_lp.unwrap().get().round_dp(2), dec!(1153.26));
        assert_eq!(btc_lp.unwrap().get().round_dp(2), dec!(26316.89));
    }

    /// Isolated long 1 BTC @ 30,000 at 10x: wallet = initial margin 3,000;
    /// MMR 0.4%, cum 0 → LP = (3000 − 30000) / (0.004 − 1) = 27,108.43…
    /// The short mirror at 10x: (3000 + 30000) / (0.004 + 1) = 32,868.5259…
    #[test]
    fn isolated_long_and_short_liquidate_on_their_side() {
        let b = one(dec!(0.004), dec!(0));
        let im = initial_margin(Notional::new(dec!(30000)), 10);
        assert_eq!(im, Notional::new(dec!(3000)));
        let long = liquidation_price(
            &pos("BTCUSDT", dec!(1), dec!(30000)),
            im,
            Notional::ZERO,
            Notional::ZERO,
            &b,
        );
        assert_eq!(long.unwrap().get().round_dp(2), dec!(27108.43));
        let short = liquidation_price(
            &pos("BTCUSDT", dec!(-1), dec!(30000)),
            im,
            Notional::ZERO,
            Notional::ZERO,
            &b,
        );
        assert_eq!(short.unwrap().get().round_dp(2), dec!(32868.53));
        // Fully collateralized long (1x): no positive mark liquidates it.
        let full = liquidation_price(
            &pos("BTCUSDT", dec!(1), dec!(30000)),
            Notional::new(dec!(30000)),
            Notional::ZERO,
            Notional::ZERO,
            &b,
        );
        assert_eq!(full, None);
    }

    /// Two brackets: below 50,000 notional 0.4% / cum 0 (50x), above 1% /
    /// cum 300 (20x; 50,000·(1% − 0.4%) = 300 keeps it continuous). A long
    /// of 2 @ 30,000 (60,000 notional) at 10x: the first bracket gives LP
    /// 27,108.43 → notional 54,216.87, in the second; with the second, LP
    /// = (6000 + 300 − 60000)/(0.02 − 2) = 27,121.21 → 54,242.42: still
    /// the second, consistent. Schedules must start at 0 and be contiguous.
    #[test]
    fn brackets_pick_the_tier_the_liquidation_notional_falls_in() {
        let b = Brackets::new(vec![
            Bracket {
                floor: Notional::ZERO,
                cap: Notional::new(dec!(50000)),
                max_leverage: 50,
                rate: dec!(0.004),
                cum: dec!(0),
            },
            Bracket {
                floor: Notional::new(dec!(50000)),
                cap: Notional::new(dec!(1000000)),
                max_leverage: 20,
                rate: dec!(0.01),
                cum: dec!(300),
            },
        ])
        .unwrap();
        assert_eq!(b.max_leverage(Notional::new(dec!(49999))), 50);
        assert_eq!(b.max_leverage(Notional::new(dec!(50000))), 20);
        assert_eq!(
            b.maintenance(Notional::new(dec!(60000))),
            Notional::new(dec!(300))
        );
        let lp = liquidation_price(
            &pos("BTCUSDT", dec!(2), dec!(30000)),
            Notional::new(dec!(6000)),
            Notional::ZERO,
            Notional::ZERO,
            &b,
        );
        assert_eq!(lp.unwrap().get().round_dp(2), dec!(27121.21));
        assert!(Brackets::new(vec![]).is_err());
        assert!(
            Brackets::new(vec![Bracket {
                floor: Notional::new(dec!(1)),
                cap: Notional::new(dec!(2)),
                max_leverage: 1,
                rate: dec!(0),
                cum: dec!(0)
            }])
            .is_err()
        );
    }

    fn held(qty: Decimal, entry: Decimal) -> Position {
        let id: InstrumentId = "etoro:KO".parse().unwrap();
        Position::from_parts(
            id,
            Qty::new(qty),
            Notional::new(qty * entry),
            Notional::ZERO,
        )
        .unwrap()
    }

    /// KO at 96.22, close-out at half the margin: a 1x short closes 50%
    /// above entry (144.33), a 5x short 10% above (105.842), a 2x long 25%
    /// below (72.165); a 1x long at half its price (48.11). Size doesn't
    /// matter: margin and loss both scale with it.
    #[test]
    fn cfd_close_out_is_where_the_loss_reaches_half_the_margin() {
        let half = CFD_CLOSE_OUT;
        assert_eq!(half, dec!(0.5));
        let at = |qty, lev| close_out_price(&held(qty, dec!(96.22)), lev, half);
        assert_eq!(at(dec!(-0.519642), 1), Some(Price::new(dec!(144.33))));
        assert_eq!(at(dec!(-3), 5), Some(Price::new(dec!(105.842))));
        assert_eq!(at(dec!(2), 2), Some(Price::new(dec!(72.165))));
        assert_eq!(at(dec!(1), 1), Some(Price::new(dec!(48.11))));
        assert_eq!(close_out_price(&held(dec!(0), dec!(96.22)), 1, half), None);
        // A long can't lose 100% of its margin above zero at 1x: none.
        assert_eq!(
            close_out_price(&held(dec!(1), dec!(96.22)), 1, dec!(1)),
            None
        );
    }
}
