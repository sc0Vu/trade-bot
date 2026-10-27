use crate::{Error, Fee, Fill, Instrument, InstrumentId, Notional, Price, Qty, Side};
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
/// [`check_against`](Self::check_against), so v2 short and perp positions
/// reuse this type. In v1 no class allows shorts and `apply` handles long
/// positions only. Changes only through [`apply`](Self::apply); restored
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

    /// Quote paid per unit held, fees included; `None` when flat.
    pub fn avg_cost(&self, instrument: &Instrument) -> Option<Price> {
        instrument.price_for(self.cost_basis, self.qty).ok()
    }

    /// Profit closed out so far, after fees.
    pub fn realized_pnl(&self) -> Notional {
        self.realized_pnl
    }

    pub fn is_flat(&self) -> bool {
        self.qty.is_zero()
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
}
