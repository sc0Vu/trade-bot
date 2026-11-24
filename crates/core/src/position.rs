use crate::{Error, Fee, Fill, Instrument, InstrumentId, Notional, Price, Qty, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Holding in one instrument, valued by average cost.
///
/// Tracks total `cost_basis` (quote paid for the units held, fees included)
/// rather than a stored average price, so repeated buys don't accumulate
/// division rounding; the average is derived on demand. Because fees sit in
/// the cost basis and are subtracted from sale proceeds, `realized_pnl` is
/// after costs.
///
/// Quantity is signed in the type: whether it may go negative is asset-class
/// policy ([`Instrument::allows_short`]), checked at runtime by
/// [`check_against`](Self::check_against). Spot positions are long only.
///
/// A margined position (a perpetual, [`crate::AssetClass::Perpetual`], or a
/// CFD, [`crate::AssetClass::Cfd`]; [`Instrument::is_margined`]) is accounted the way
/// the venue does: `cost_basis` is the signed entry notional (entry price ×
/// signed qty, no fees), so `avg_cost` is the entry price the venue's
/// liquidation maths uses; fees and funding go straight to `realized_pnl`.
/// A fill against the position closes it first (P&L = units closed × the
/// move from entry, signed), and what's left opens the other way at the
/// fill price. Changes only through [`apply`](Self::apply); restored
/// from storage only through [`from_parts`](Self::from_parts), which
/// deserializing also goes through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PositionParts", into = "PositionParts")]
pub struct Position {
    instrument: InstrumentId,
    qty: Qty,
    cost_basis: Notional,
    realized_pnl: Notional,
}

impl Position {
    /// No holding and no realized P&L yet.
    pub fn flat(instrument: InstrumentId) -> Self {
        Self {
            instrument,
            qty: Qty::ZERO,
            cost_basis: Notional::ZERO,
            realized_pnl: Notional::ZERO,
        }
    }

    /// Rebuilds a position saved earlier (database, snapshot).
    ///
    /// Checks only rules that hold for any sign: a cost basis never has the
    /// opposite sign to qty, and a flat position has none. Does not reject a
    /// negative qty; that is asset-class policy, checked by
    /// [`check_against`](Self::check_against) once the instrument is known.
    pub fn from_parts(
        instrument: InstrumentId,
        qty: Qty,
        cost_basis: Notional,
        realized_pnl: Notional,
    ) -> Result<Self, Error> {
        if (qty.is_positive() && cost_basis.is_negative())
            || (qty.is_negative() && cost_basis.is_positive())
        {
            return Err(Error::InvalidPosition(
                "cost basis sign must match qty sign",
            ));
        }
        if qty.is_zero() && !cost_basis.is_zero() {
            return Err(Error::InvalidPosition(
                "flat position must have zero cost basis",
            ));
        }
        Ok(Self {
            instrument,
            qty,
            cost_basis,
            realized_pnl,
        })
    }

    /// Checks this position against `instrument`'s asset-class policy: same
    /// instrument, and no negative qty unless the class allows shorts. Run
    /// it on any position restored from outside (`from_parts`, serde) before
    /// trading on it.
    pub fn check_against(&self, instrument: &Instrument) -> Result<(), Error> {
        if instrument.id != self.instrument {
            return Err(Error::InvalidPosition(
                "position is for a different instrument",
            ));
        }
        if self.qty.is_negative() && !instrument.allows_short() {
            return Err(Error::InvalidPosition(
                "asset class can't hold a negative position",
            ));
        }
        Ok(())
    }

    pub fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    /// Base units held.
    pub fn qty(&self) -> Qty {
        self.qty
    }

    /// Quote paid for the units held, fees included.
    pub fn cost_basis(&self) -> Notional {
        self.cost_basis
    }

    /// Quote paid per unit held, fees included; for a perpetual, the entry
    /// price (long or short). `None` when flat. Basis and qty share a sign,
    /// so their magnitudes give the price either way.
    pub fn avg_cost(&self, instrument: &Instrument) -> Option<Price> {
        instrument
            .price_for(
                Notional::new(self.cost_basis.get().abs()),
                Qty::new(self.qty.get().abs()),
            )
            .ok()
    }

    /// Profit closed out so far, after fees.
    pub fn realized_pnl(&self) -> Notional {
        self.realized_pnl
    }

    pub fn is_flat(&self) -> bool {
        self.qty.is_zero()
    }

    /// [`apply`](Self::apply) for a margined position (perpetual, CFD);
    /// see the type docs.
    fn apply_perp(&mut self, fill: &Fill) -> Result<(), Error> {
        let fee = match &fill.fee {
            Fee::Quote(f) | Fee::Other { value: Some(f), .. } => *f,
            Fee::Base(_) => {
                return Err(Error::PositionUpdate(
                    "a margined position's fee is paid in the quote, not the base",
                ));
            }
            Fee::Other { value: None, .. } => {
                return Err(Error::PositionUpdate("fee in another asset has no value"));
            }
        };
        let signed = match fill.side {
            Side::Buy => fill.qty.get(),
            Side::Sell => -fill.qty.get(),
        };
        let held = self.qty.get();
        let price = fill.price.get();
        let mut realized = -fee.get();
        let mut qty = held;
        let mut basis = self.cost_basis.get();
        if held.is_zero() || held.is_sign_positive() == signed.is_sign_positive() {
            // Opening or adding: entry notional grows at the fill price.
            qty += signed;
            basis += signed * price;
        } else {
            // Against the position: close up to all of it first.
            let closing = signed.abs().min(held.abs());
            // The closed units' share of the entry notional: all of it on a
            // full close (no division, so no rounding), pro rata otherwise.
            let share = if closing == held.abs() {
                basis
            } else {
                basis * closing / held.abs()
            };
            // closed × (price − entry) × side = side × closed × price − share.
            realized += sign(held) * closing * price - share;
            qty = held - closing * sign(held);
            basis -= share;
            let rest = signed.abs() - closing;
            if rest.is_sign_positive() && !rest.is_zero() {
                // Flip: what's left opens the other way at the fill price.
                qty = rest * sign(signed);
                basis = qty * price;
            }
        }
        self.qty = Qty::new(qty);
        self.cost_basis = Notional::new(basis);
        self.realized_pnl += Notional::new(realized);
        Ok(())
    }

    /// Books an amount the venue settled outside any fill (a funding
    /// payment as it booked it: positive received, negative paid) into
    /// realized P&L. Position and basis are untouched.
    pub fn credit(&mut self, amount: Notional) {
        self.realized_pnl += amount;
    }

    /// Books a dividend adjustment on a CFD position at the ex-dividend
    /// date: a long receives `qty × per_unit`, a short pays it (the venue
    /// passes on what an owner would get; eToro: "you pay any eligible
    /// dividends if you hold a short"). `per_unit` is what the venue
    /// credits a long per unit (after any tax it withholds). Returns the
    /// amount received (negative: paid). Flat: nothing.
    pub fn apply_dividend(&mut self, per_unit: Decimal) -> Notional {
        let received = Notional::new(self.qty.get() * per_unit);
        self.realized_pnl += received;
        received
    }

    /// Settles one funding payment on a perpetual position: the venue
    /// charges `qty × mark × rate` (a positive rate: longs pay shorts),
    /// added to realized P&L. Returns the amount received (negative: paid).
    /// Flat: nothing.
    pub fn apply_funding(&mut self, rate: Decimal, mark: Price) -> Notional {
        let paid = Notional::new(-(self.qty.get() * mark.get() * rate));
        self.realized_pnl += paid;
        paid
    }

    /// Applies one fill at average cost.
    ///
    /// Buy: units received are added; their cost (notional, plus a quote fee)
    /// joins the cost basis. A base fee instead reduces units received, which
    /// raises the average the same way.
    ///
    /// Sell: units leaving are the fill qty plus any base fee. They take a
    /// proportional share of the cost basis (all of it when the sale empties
    /// the position, so no rounding dust is left). Realized P&L gains
    /// proceeds (notional minus a quote fee) less that share. The average
    /// cost of what remains is unchanged.
    ///
    /// Rejected, leaving the position unchanged: a fill for another
    /// instrument, a position that is already negative (short accounting is
    /// v2), a sell larger than the holding (no v1 class allows shorts), a
    /// base fee that consumes the whole buy, and a `Fee::Other` without a
    /// quote `value` (ignoring it would overstate P&L). A valued
    /// `Fee::Other` counts exactly like a quote fee of `value`: the buy
    /// receives the full quantity, since the fee came out of another
    /// asset.
    pub fn apply(&mut self, fill: &Fill, instrument: &Instrument) -> Result<(), Error> {
        if fill.instrument != self.instrument || instrument.id != self.instrument {
            return Err(Error::PositionUpdate("fill is for a different instrument"));
        }
        if instrument.is_margined() {
            return self.apply_perp(fill);
        }
        if self.qty.is_negative() {
            return Err(Error::PositionUpdate(
                "short position accounting not supported in v1",
            ));
        }
        let gross = instrument.notional(fill.qty, fill.price);
        match fill.side {
            Side::Buy => {
                let (received, cost) = match &fill.fee {
                    Fee::Quote(f) | Fee::Other { value: Some(f), .. } => (fill.qty, gross + *f),
                    Fee::Base(b) if *b < fill.qty => (fill.qty - *b, gross),
                    Fee::Base(_) => {
                        return Err(Error::PositionUpdate("base fee consumes the whole buy"));
                    }
                    Fee::Other { value: None, .. } => {
                        return Err(Error::PositionUpdate("fee in another asset has no value"));
                    }
                };
                self.qty += received;
                self.cost_basis += cost;
            }
            Side::Sell => {
                let (removed, proceeds) = match &fill.fee {
                    Fee::Quote(f) | Fee::Other { value: Some(f), .. } => (fill.qty, gross - *f),
                    Fee::Base(b) => (fill.qty + *b, gross),
                    Fee::Other { value: None, .. } => {
                        return Err(Error::PositionUpdate("fee in another asset has no value"));
                    }
                };
                if removed > self.qty {
                    return Err(Error::PositionUpdate("sell exceeds holding"));
                }
                let cost_removed = if removed == self.qty {
                    self.cost_basis
                } else {
                    self.cost_basis * (removed / self.qty)
                };
                self.realized_pnl += proceeds - cost_removed;
                self.qty -= removed;
                self.cost_basis -= cost_removed;
            }
        }
        Ok(())
    }
}

/// +1 or −1: the direction of a non-zero quantity.
fn sign(d: Decimal) -> Decimal {
    if d.is_sign_negative() {
        -Decimal::ONE
    } else {
        Decimal::ONE
    }
}

/// Serde shape of [`Position`]; deserializing goes through `from_parts`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionParts {
    instrument: InstrumentId,
    qty: Qty,
    cost_basis: Notional,
    realized_pnl: Notional,
}

impl TryFrom<PositionParts> for Position {
    type Error = Error;
    fn try_from(p: PositionParts) -> Result<Self, Error> {
        Self::from_parts(p.instrument, p.qty, p.cost_basis, p.realized_pnl)
    }
}

impl From<Position> for PositionParts {
    fn from(p: Position) -> Self {
        Self {
            instrument: p.instrument,
            qty: p.qty,
            cost_basis: p.cost_basis,
            realized_pnl: p.realized_pnl,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Asset, AssetClass, FillSpec, InstrumentSpec, Liquidity, OrderId, Timestamp};
    use rust_decimal_macros::dec;

    fn btcusdt() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance:BTCUSDT".parse().unwrap(),
            class: AssetClass::Spot,
            base: Asset::new("BTC").unwrap(),
            quote: Asset::new("USDT").unwrap(),
            tick_size: Price::new(dec!(0.01)),
            lot_size: Qty::new(dec!(0.00001)),
            min_qty: Qty::new(dec!(0.00001)),
            min_notional: Notional::new(dec!(5)),
            max_open_orders: None,
            max_open_stops: None,
            price_band: None,
        })
        .unwrap()
    }

    fn fill(side: Side, qty: Qty, price: Price, fee: Fee) -> Fill {
        Fill::new(FillSpec {
            order_id: OrderId::new("t").unwrap(),
            instrument: "binance:BTCUSDT".parse().unwrap(),
            side,
            qty,
            price,
            fee,
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    fn q(d: rust_decimal::Decimal) -> Qty {
        Qty::new(d)
    }
    fn p(d: rust_decimal::Decimal) -> Price {
        Price::new(d)
    }
    fn n(d: rust_decimal::Decimal) -> Notional {
        Notional::new(d)
    }

    #[test]
    fn flat_position_is_all_zero() {
        let pos = Position::flat("binance:BTCUSDT".parse().unwrap());
        assert!(pos.is_flat());
        assert_eq!(pos.cost_basis(), Notional::ZERO);
        assert_eq!(pos.avg_cost(&btcusdt()), None);
        assert_eq!(pos.realized_pnl(), Notional::ZERO);
    }

    /// Two buys, a partial sell, a closing sell; quote fees at 0.1%.
    /// Expected values worked by hand:
    ///   buy 1 @ 100, fee 0.1    -> qty 1,   cost 100.1,   avg 100.1
    ///   buy 1 @ 200, fee 0.2    -> qty 2,   cost 300.3,   avg 150.15
    ///   sell 0.5 @ 300, fee 0.15 -> proceeds 149.85, cost out 300.3/4 = 75.075,
    ///                               realized 74.775, qty 1.5, cost 225.225, avg 150.15
    ///   sell 1.5 @ 100, fee 0.15 -> proceeds 149.85, cost out 225.225,
    ///                               realized 74.775 - 75.375 = -0.6, flat
    /// Cross-check by cash flow: paid 100.1 + 200.2 = 300.3, received
    /// 149.85 + 149.85 = 299.7, net -0.6.
    #[test]
    fn average_cost_round_trip_with_quote_fees() {
        let i = btcusdt();
        let mut pos = Position::flat(i.id.clone());

        pos.apply(
            &fill(
                Side::Buy,
                q(dec!(1)),
                p(dec!(100)),
                Fee::Quote(n(dec!(0.1))),
            ),
            &i,
        )
        .unwrap();
        assert_eq!((pos.qty(), pos.cost_basis()), (q(dec!(1)), n(dec!(100.1))));
        assert_eq!(pos.avg_cost(&i), Some(p(dec!(100.1))));

        pos.apply(
            &fill(
                Side::Buy,
                q(dec!(1)),
                p(dec!(200)),
                Fee::Quote(n(dec!(0.2))),
            ),
            &i,
        )
        .unwrap();
        assert_eq!((pos.qty(), pos.cost_basis()), (q(dec!(2)), n(dec!(300.3))));
        assert_eq!(pos.avg_cost(&i), Some(p(dec!(150.15))));

        pos.apply(
            &fill(
                Side::Sell,
                q(dec!(0.5)),
                p(dec!(300)),
                Fee::Quote(n(dec!(0.15))),
            ),
            &i,
        )
        .unwrap();
        assert_eq!(pos.realized_pnl(), n(dec!(74.775)));
        assert_eq!(
            (pos.qty(), pos.cost_basis()),
            (q(dec!(1.5)), n(dec!(225.225)))
        );
        assert_eq!(pos.avg_cost(&i), Some(p(dec!(150.15))));

        pos.apply(
            &fill(
                Side::Sell,
                q(dec!(1.5)),
                p(dec!(100)),
                Fee::Quote(n(dec!(0.15))),
            ),
            &i,
        )
        .unwrap();
        assert_eq!(pos.realized_pnl(), n(dec!(-0.6)));
        assert!(pos.is_flat());
        assert_eq!(pos.cost_basis(), Notional::ZERO);
    }

    /// Binance's usual spot fee: buy charged in base, sell in quote.
    ///   buy 1 @ 60000, fee 0.001 BTC -> qty 0.999, cost 60000,
    ///                                   avg 60000 / 0.999 = 60060.0600...
    ///   sell 0.999 @ 61000, fee 0.1% of 60939 = 60.939 USDT
    ///                                -> proceeds 60878.061, realized 878.061
    #[test]
    fn base_fee_on_buy_reduces_units_not_cost() {
        let i = btcusdt();
        let mut pos = Position::flat(i.id.clone());

        pos.apply(
            &fill(
                Side::Buy,
                q(dec!(1)),
                p(dec!(60000)),
                Fee::Base(q(dec!(0.001))),
            ),
            &i,
        )
        .unwrap();
        assert_eq!(
            (pos.qty(), pos.cost_basis()),
            (q(dec!(0.999)), n(dec!(60000)))
        );
        let avg = pos.avg_cost(&i).unwrap();
        assert!(avg > p(dec!(60060.06)) && avg < p(dec!(60060.07)));

        let sell = fill(
            Side::Sell,
            q(dec!(0.999)),
            p(dec!(61000)),
            Fee::Quote(n(dec!(60.939))),
        );
        pos.apply(&sell, &i).unwrap();
        assert_eq!(pos.realized_pnl(), n(dec!(878.061)));
        assert!(pos.is_flat());
    }

    /// Hold 1 BTC at cost 100. Sell 0.5 @ 200 with 0.001 BTC fee:
    /// 0.501 BTC leaves, taking 100 * 0.501 = 50.1 of cost; proceeds 100;
    /// realized 49.9; left 0.499 BTC at cost 49.9.
    #[test]
    fn base_fee_on_sell_removes_extra_units() {
        let i = btcusdt();
        let mut pos = Position::flat(i.id.clone());
        pos.apply(
            &fill(
                Side::Buy,
                q(dec!(1)),
                p(dec!(100)),
                Fee::Quote(Notional::ZERO),
            ),
            &i,
        )
        .unwrap();

        pos.apply(
            &fill(
                Side::Sell,
                q(dec!(0.5)),
                p(dec!(200)),
                Fee::Base(q(dec!(0.001))),
            ),
            &i,
        )
        .unwrap();
        assert_eq!(pos.realized_pnl(), n(dec!(49.9)));
        assert_eq!(
            (pos.qty(), pos.cost_basis()),
            (q(dec!(0.499)), n(dec!(49.9)))
        );
    }

    #[test]
    fn rejected_fills_leave_position_unchanged() {
        let i = btcusdt();
        let mut pos = Position::flat(i.id.clone());
        pos.apply(
            &fill(
                Side::Buy,
                q(dec!(1)),
                p(dec!(100)),
                Fee::Quote(Notional::ZERO),
            ),
            &i,
        )
        .unwrap();
        let before = pos.clone();

        let oversell = fill(
            Side::Sell,
            q(dec!(1.1)),
            p(dec!(100)),
            Fee::Quote(Notional::ZERO),
        );
        assert!(pos.apply(&oversell, &i).is_err());
        // Selling all units plus a base fee also oversells.
        let fee_oversell = fill(
            Side::Sell,
            q(dec!(1)),
            p(dec!(100)),
            Fee::Base(q(dec!(0.001))),
        );
        assert!(pos.apply(&fee_oversell, &i).is_err());

        let bnb = Fee::Other {
            asset: Asset::new("BNB").unwrap(),
            amount: q(dec!(0.01)),
            value: None,
        };
        assert!(
            pos.apply(&fill(Side::Buy, q(dec!(1)), p(dec!(100)), bnb), &i)
                .is_err()
        );

        let eaten = fill(
            Side::Buy,
            q(dec!(0.001)),
            p(dec!(100)),
            Fee::Base(q(dec!(0.001))),
        );
        assert!(pos.apply(&eaten, &i).is_err());

        let mut other = Position::flat("binance:ETHUSDT".parse().unwrap());
        let buy = fill(
            Side::Buy,
            q(dec!(1)),
            p(dec!(100)),
            Fee::Quote(Notional::ZERO),
        );
        assert!(other.apply(&buy, &i).is_err());

        assert_eq!(pos, before);
    }

    #[test]
    fn from_parts_accepts_reachable_state() {
        let id: InstrumentId = "binance:BTCUSDT".parse().unwrap();
        let pos =
            Position::from_parts(id, q(dec!(1.5)), n(dec!(225.225)), n(dec!(74.775))).unwrap();
        assert_eq!(pos.avg_cost(&btcusdt()), Some(p(dec!(150.15))));
        // Flat with history: zero units, zero basis, P&L kept.
        let id: InstrumentId = "binance:BTCUSDT".parse().unwrap();
        assert!(Position::from_parts(id, Qty::ZERO, Notional::ZERO, n(dec!(-0.6))).is_ok());
    }

    /// The type admits a short (negative qty, negative basis): sign is
    /// asset-class policy, not a type rule (CLAUDE.md).
    #[test]
    fn from_parts_admits_signed_qty() {
        let id: InstrumentId = "binance:BTCUSDT".parse().unwrap();
        let short = Position::from_parts(id, q(dec!(-1)), n(dec!(-100)), Notional::ZERO).unwrap();
        assert_eq!(short.qty(), q(dec!(-1)));
    }

    #[test]
    fn from_parts_rejects_inconsistent_state() {
        let id = || -> InstrumentId { "binance:BTCUSDT".parse().unwrap() };
        assert!(Position::from_parts(id(), q(dec!(1)), n(dec!(-1)), Notional::ZERO).is_err());
        assert!(Position::from_parts(id(), q(dec!(-1)), n(dec!(100)), Notional::ZERO).is_err());
        assert!(Position::from_parts(id(), Qty::ZERO, n(dec!(5)), Notional::ZERO).is_err());
    }

    #[test]
    fn check_against_enforces_spot_policy() {
        let i = btcusdt();
        let long = Position::from_parts(i.id.clone(), q(dec!(1)), n(dec!(100)), Notional::ZERO);
        assert_eq!(long.unwrap().check_against(&i), Ok(()));

        let short = Position::from_parts(i.id.clone(), q(dec!(-1)), n(dec!(-100)), Notional::ZERO);
        assert!(short.unwrap().check_against(&i).is_err());

        let eth = Position::flat("binance:ETHUSDT".parse().unwrap());
        assert!(eth.check_against(&i).is_err());
    }

    #[test]
    fn apply_refuses_a_negative_position() {
        let i = btcusdt();
        let mut short =
            Position::from_parts(i.id.clone(), q(dec!(-1)), n(dec!(-100)), Notional::ZERO).unwrap();
        let before = short.clone();
        let buy = fill(
            Side::Buy,
            q(dec!(1)),
            p(dec!(100)),
            Fee::Quote(Notional::ZERO),
        );
        assert!(short.apply(&buy, &i).is_err());
        assert_eq!(short, before);
    }

    #[test]
    fn deserialize_validates() {
        let ok =
            r#"{"instrument":"binance:BTCUSDT","qty":"1","cost_basis":"100","realized_pnl":"0"}"#;
        let pos: Position = serde_json::from_str(ok).unwrap();
        assert_eq!(serde_json::to_string(&pos).unwrap(), ok);
        let mismatched = ok.replace(r#""qty":"1""#, r#""qty":"-1""#);
        assert!(serde_json::from_str::<Position>(&mismatched).is_err());
        let short = mismatched.replace(r#""cost_basis":"100""#, r#""cost_basis":"-100""#);
        assert!(serde_json::from_str::<Position>(&short).is_ok());
    }

    fn bnb(amount: rust_decimal::Decimal, value: rust_decimal::Decimal) -> Fee {
        Fee::Other {
            asset: Asset::new("BNB").unwrap(),
            amount: q(amount),
            value: Some(n(value)),
        }
    }

    /// Hand-worked: buy 2 @ 100 with a BNB fee worth 0.12 USDT -> qty 2
    /// (nothing withheld in BTC), cost 200.12. Sell 0.5 @ 110 with a BNB
    /// fee worth 0.033 -> proceeds 54.967, cost removed 200.12 * 0.25 =
    /// 50.03, realized 4.937; left 1.5 at cost 150.09.
    #[test]
    fn valued_third_asset_fee_counts_like_a_quote_fee() {
        let i = btcusdt();
        let mut pos = Position::flat(i.id.clone());
        pos.apply(
            &fill(
                Side::Buy,
                q(dec!(2)),
                p(dec!(100)),
                bnb(dec!(0.0002), dec!(0.12)),
            ),
            &i,
        )
        .unwrap();
        assert_eq!((pos.qty(), pos.cost_basis()), (q(dec!(2)), n(dec!(200.12))));
        pos.apply(
            &fill(
                Side::Sell,
                q(dec!(0.5)),
                p(dec!(110)),
                bnb(dec!(0.000055), dec!(0.033)),
            ),
            &i,
        )
        .unwrap();
        assert_eq!(pos.realized_pnl(), n(dec!(4.937)));
        assert_eq!(
            (pos.qty(), pos.cost_basis()),
            (q(dec!(1.5)), n(dec!(150.09)))
        );
    }

    #[test]
    fn negative_fee_value_is_rejected() {
        let spec = FillSpec {
            fee: bnb(dec!(0.0001), dec!(-0.06)),
            ..fill(
                Side::Buy,
                q(dec!(1)),
                p(dec!(100)),
                Fee::Quote(Notional::ZERO),
            )
            .into()
        };
        assert!(Fill::new(spec).is_err());
    }

    fn perp() -> Instrument {
        Instrument::new(InstrumentSpec {
            id: "binance:BTCUSDT".parse().unwrap(),
            class: AssetClass::Perpetual,
            base: crate::Asset::new("BTC").unwrap(),
            quote: crate::Asset::new("USDT").unwrap(),
            tick_size: Price::new(dec!(0.1)),
            lot_size: Qty::new(dec!(0.001)),
            min_qty: Qty::new(dec!(0.001)),
            min_notional: Notional::new(dec!(5)),
            price_band: None,
            max_open_orders: None,
            max_open_stops: None,
        })
        .unwrap()
    }

    fn perp_fill(side: Side, qty: Decimal, price: Decimal, fee: Decimal) -> Fill {
        Fill::new(FillSpec {
            order_id: OrderId::new("o-1").unwrap(),
            instrument: perp().id.clone(),
            side,
            qty: Qty::new(qty),
            price: Price::new(price),
            fee: Fee::Quote(Notional::new(fee)),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    /// Worked by hand:
    /// 1. Buy 2 @ 100, fee 0.2: long 2, entry notional 200, realized −0.2.
    /// 2. Buy 1 @ 130: long 3, 330 (entry 110).
    /// 3. Sell 1 @ 120, fee 0.12: closes 1 at +10 → realized −0.2 + 10 −
    ///    0.12 = 9.68; long 2, 220 (entry still 110).
    /// 4. Sell 5 @ 90: closes 2 at −20 each → realized 9.68 − 40 = −30.32;
    ///    flips: short 3 at 90, −270.
    /// 5. Buy 3 @ 80: the short gains 10 each → realized −0.32; flat.
    #[test]
    fn a_perpetual_opens_adds_reduces_flips_and_closes() {
        let p = perp();
        let mut pos = Position::flat(p.id.clone());
        let step =
            |pos: &mut Position, side, q, px, fee, qty: Decimal, basis: Decimal, real: Decimal| {
                pos.apply(&perp_fill(side, q, px, fee), &p).unwrap();
                assert_eq!(
                    (
                        pos.qty().get(),
                        pos.cost_basis().get(),
                        pos.realized_pnl().get()
                    ),
                    (qty, basis, real)
                );
            };
        step(
            &mut pos,
            Side::Buy,
            dec!(2),
            dec!(100),
            dec!(0.2),
            dec!(2),
            dec!(200),
            dec!(-0.2),
        );
        step(
            &mut pos,
            Side::Buy,
            dec!(1),
            dec!(130),
            dec!(0),
            dec!(3),
            dec!(330),
            dec!(-0.2),
        );
        assert_eq!(pos.avg_cost(&p), Some(Price::new(dec!(110))));
        step(
            &mut pos,
            Side::Sell,
            dec!(1),
            dec!(120),
            dec!(0.12),
            dec!(2),
            dec!(220),
            dec!(9.68),
        );
        step(
            &mut pos,
            Side::Sell,
            dec!(5),
            dec!(90),
            dec!(0),
            dec!(-3),
            dec!(-270),
            dec!(-30.32),
        );
        assert_eq!(pos.avg_cost(&p), Some(Price::new(dec!(90))));
        pos.check_against(&p).unwrap();
        step(
            &mut pos,
            Side::Buy,
            dec!(3),
            dec!(80),
            dec!(0),
            dec!(0),
            dec!(0),
            dec!(-0.32),
        );
        assert!(pos.is_flat());
    }

    /// Short 1 @ 100 and 2 @ 101: entry notional −302, entry price
    /// 100.666… (repeating). Buying all 3 back @ 100 realizes exactly
    /// 302 − 300 = 2: a full close takes the whole basis, no division
    /// (Binance reported such a round trip to the cent; a divided entry
    /// left 28-digit dust).
    #[test]
    fn a_full_perpetual_close_realizes_exactly() {
        let p = perp();
        let mut pos = Position::flat(p.id.clone());
        pos.apply(&perp_fill(Side::Sell, dec!(1), dec!(100), dec!(0)), &p)
            .unwrap();
        pos.apply(&perp_fill(Side::Sell, dec!(2), dec!(101), dec!(0)), &p)
            .unwrap();
        pos.apply(&perp_fill(Side::Buy, dec!(3), dec!(100), dec!(0)), &p)
            .unwrap();
        assert_eq!(pos.realized_pnl(), Notional::new(dec!(2)));
        assert!(pos.is_flat());
        assert_eq!(pos.cost_basis(), Notional::ZERO);
    }

    /// Long 2 at mark 100, rate 0.0001: pays 0.02. Short 3, same: receives
    /// 0.03. A negative rate reverses both.
    #[test]
    fn funding_is_paid_by_longs_when_the_rate_is_positive() {
        let p = perp();
        let mut long = Position::flat(p.id.clone());
        long.apply(&perp_fill(Side::Buy, dec!(2), dec!(100), dec!(0)), &p)
            .unwrap();
        assert_eq!(
            long.apply_funding(dec!(0.0001), Price::new(dec!(100))),
            Notional::new(dec!(-0.02))
        );
        assert_eq!(long.realized_pnl(), Notional::new(dec!(-0.02)));
        let mut short = Position::flat(p.id.clone());
        short
            .apply(&perp_fill(Side::Sell, dec!(3), dec!(100), dec!(0)), &p)
            .unwrap();
        assert_eq!(
            short.apply_funding(dec!(0.0001), Price::new(dec!(100))),
            Notional::new(dec!(0.03))
        );
        assert_eq!(
            short.apply_funding(dec!(-0.0001), Price::new(dec!(100))),
            Notional::new(dec!(-0.03))
        );
    }

    /// A perpetual pays fees in the quote: a base-asset fee is refused, and
    /// spot accounting is untouched by all this (a spot short is still
    /// refused by `check_against`).
    #[test]
    fn perpetual_fees_are_quote_and_spot_stays_long_only() {
        let p = perp();
        let mut pos = Position::flat(p.id.clone());
        let mut base_fee = perp_fill(Side::Buy, dec!(1), dec!(100), dec!(0));
        base_fee = Fill::new(FillSpec {
            fee: Fee::Base(Qty::new(dec!(0.001))),
            ..base_fee.into()
        })
        .unwrap();
        assert!(pos.apply(&base_fee, &p).is_err());
        assert!(p.allows_short());
        let spot = Instrument::new(InstrumentSpec {
            class: AssetClass::Spot,
            ..p.into()
        })
        .unwrap();
        let short = Position::from_parts(
            spot.id.clone(),
            Qty::new(dec!(-1)),
            Notional::new(dec!(-100)),
            Notional::ZERO,
        )
        .unwrap();
        assert!(short.check_against(&spot).is_err());
    }

    /// A CFD on BTCUSDT's id (the helpers fill that id), eToro-like
    /// filters: tick 0.01, unit 1e-6.
    fn cfd() -> Instrument {
        Instrument::new(InstrumentSpec {
            class: AssetClass::Cfd,
            tick_size: Price::new(dec!(0.01)),
            lot_size: Qty::new(dec!(0.000001)),
            min_qty: Qty::new(dec!(0.000001)),
            ..perp().into()
        })
        .unwrap()
    }

    /// The eToro demo probe of 2026-11-23 (KO short CFD, 1x): sell
    /// 0.519642 at 96.22, fee 0.07; buy back 0.25 at 96.26, fee 0.035
    /// (derived from the proceeds). eToro reported the closed part's net
    /// profit as −0.01 (fees apart) and the rest as 0.269642 units with
    /// $25.94 of margin: 0.269642 × 96.22 = 25.94495324.
    #[test]
    fn a_cfd_short_is_accounted_as_the_venue_reports_it() {
        let inst = cfd();
        assert!(inst.allows_short() && inst.is_margined());
        let mut ko = Position::flat(inst.id.clone());
        ko.apply(
            &fill(
                Side::Sell,
                q(dec!(0.519642)),
                p(dec!(96.22)),
                Fee::Quote(Notional::new(dec!(0.07))),
            ),
            &inst,
        )
        .unwrap();
        assert_eq!(ko.qty(), q(dec!(-0.519642)));
        assert_eq!(
            ko.realized_pnl(),
            Notional::new(dec!(-0.07)),
            "the opening fee"
        );
        ko.apply(
            &fill(
                Side::Buy,
                q(dec!(0.25)),
                p(dec!(96.26)),
                Fee::Quote(Notional::new(dec!(0.035))),
            ),
            &inst,
        )
        .unwrap();
        assert_eq!(ko.qty(), q(dec!(-0.269642)));
        assert_eq!(ko.cost_basis(), Notional::new(dec!(-25.94495324)));
        // −0.07 − 0.035 fees, and 0.25 × (96.22 − 96.26) = −0.01.
        assert_eq!(ko.realized_pnl(), Notional::new(dec!(-0.115)));
    }

    /// A short pays the dividend, a long receives it: 10 units short at a
    /// 0.51 dividend → −5.10; 4 units long → +2.04. Position untouched.
    #[test]
    fn dividends_are_paid_by_shorts_and_received_by_longs() {
        let inst = cfd();
        let mut short = Position::flat(inst.id.clone());
        short
            .apply(
                &fill(
                    Side::Sell,
                    q(dec!(10)),
                    p(dec!(60)),
                    Fee::Quote(Notional::ZERO),
                ),
                &inst,
            )
            .unwrap();
        assert_eq!(short.apply_dividend(dec!(0.51)), Notional::new(dec!(-5.10)));
        assert_eq!(short.realized_pnl(), Notional::new(dec!(-5.10)));
        assert_eq!(short.qty(), q(dec!(-10)));
        assert_eq!(short.cost_basis(), Notional::new(dec!(-600)));
        let mut long = Position::flat(inst.id.clone());
        long.apply(
            &fill(
                Side::Buy,
                q(dec!(4)),
                p(dec!(60)),
                Fee::Quote(Notional::ZERO),
            ),
            &inst,
        )
        .unwrap();
        assert_eq!(long.apply_dividend(dec!(0.51)), Notional::new(dec!(2.04)));
        let mut flat = Position::flat(inst.id.clone());
        assert_eq!(flat.apply_dividend(dec!(0.51)), Notional::ZERO);
    }
}
