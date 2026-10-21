use crate::{Asset, Error, FilterError, InstrumentId, Notional, Price, Qty, Side};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

/// Asset class. Decides which math `Instrument` applies.
///
/// v1 trades `Spot` only. `UsEquity` exists now so adding equities is
/// additive and never reshapes callers; options are deferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AssetClass {
    Spot,
    UsEquity,
    // UsOption (deferred)
}

/// When a fill's units and cash become usable again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// Usable at once (crypto spot).
    Instant,
    /// Usable after N business days (US equities: T+1 since May 2024).
    TPlus(u8),
}

/// Unchecked instrument reference data, as read from a venue or config.
/// Turn it into an [`Instrument`] with [`Instrument::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstrumentSpec {
    pub id: InstrumentId,
    pub class: AssetClass,
    /// Asset bought or sold (`BTC`); for an equity, the ticker.
    pub base: Asset,
    /// Asset prices are quoted in, and fees and P&L are measured in.
    pub quote: Asset,
    /// Minimum price increment; every valid price is a multiple of it.
    pub tick_size: Price,
    /// Minimum quantity increment (Binance `stepSize`).
    pub lot_size: Qty,
    /// Smallest quantity the venue accepts.
    pub min_qty: Qty,
    /// Smallest `qty * price` the venue accepts.
    pub min_notional: Notional,
}

/// Tradable instrument plus all of its asset-class math (invariant 4).
///
/// Brokers and strategies never compute notional or round to venue filters
/// themselves; they ask the instrument. Filters live here so an order the
/// venue would reject is caught before submit, the same way in backtest and
/// live. Always valid: `tick_size` and `lot_size` are positive, minimums are
/// non-negative, and `base != quote`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "InstrumentSpec", into = "InstrumentSpec")]
pub struct Instrument(InstrumentSpec);

impl Instrument {
    /// Validates reference data. Rejects a non-positive tick or lot (rounding
    /// would divide by zero), negative minimums, and `base == quote`.
    pub fn new(spec: InstrumentSpec) -> Result<Self, Error> {
        if !spec.tick_size.is_positive() {
            return Err(Error::InvalidInstrument("tick_size must be positive"));
        }
        if !spec.lot_size.is_positive() {
            return Err(Error::InvalidInstrument("lot_size must be positive"));
        }
        if spec.min_qty.is_negative() {
            return Err(Error::InvalidInstrument("min_qty must be non-negative"));
        }
        if spec.min_notional.is_negative() {
            return Err(Error::InvalidInstrument(
                "min_notional must be non-negative",
            ));
        }
        if spec.base == spec.quote {
            return Err(Error::InvalidInstrument("base and quote must differ"));
        }
        Ok(Self(spec))
    }

    /// Value of `qty` units at `price`, in quote currency.
    ///
    /// Spot and equity: `qty * price`. Options will multiply by contract
    /// size; that is why this lives here and `Qty * Price` does not exist.
    pub fn notional(&self, qty: Qty, price: Price) -> Notional {
        match self.class {
            AssetClass::Spot | AssetClass::UsEquity => Notional::new(qty.get() * price.get()),
        }
    }

    /// Units that `notional` buys at `price`, inverse of [`notional`](Self::notional).
    /// Not rounded: pass the result through [`round_qty`](Self::round_qty)
    /// before ordering.
    pub fn qty_for(&self, notional: Notional, price: Price) -> Result<Qty, FilterError> {
        if !price.is_positive() {
            return Err(FilterError::NonPositivePrice);
        }
        match self.class {
            AssetClass::Spot | AssetClass::UsEquity => Ok(Qty::new(notional.get() / price.get())),
        }
    }

    /// Spot settles instantly; US equities at T+1.
    pub fn settlement(&self) -> Settlement {
        match self.class {
            AssetClass::Spot => Settlement::Instant,
            AssetClass::UsEquity => Settlement::TPlus(1),
        }
    }

    /// Rounds `qty` toward zero to a multiple of `lot_size`. Never away from
    /// zero: rounding up could sell or spend more than the balance holds.
    pub fn round_qty(&self, qty: Qty) -> Qty {
        let lot = self.lot_size.get();
        Qty::new((qty.get() / lot).trunc() * lot)
    }

    /// Rounds `price` to a multiple of `tick_size`, toward the passive side:
    /// down for a buy, up for a sell. Rounding then never makes a limit order
    /// more aggressive than the strategy asked.
    pub fn round_price(&self, price: Price, side: Side) -> Price {
        let tick = self.tick_size.get();
        let steps = price.get() / tick;
        let steps = match side {
            Side::Buy => steps.floor(),
            Side::Sell => steps.ceil(),
        };
        Price::new(steps * tick)
    }

    /// Checks `qty` at `price` against every venue filter. Run on the final,
    /// rounded order; returns the first rule broken.
    pub fn check_order(&self, qty: Qty, price: Price) -> Result<(), FilterError> {
        if !qty.is_positive() {
            return Err(FilterError::NonPositiveQty);
        }
        if !price.is_positive() {
            return Err(FilterError::NonPositivePrice);
        }
        if qty < self.min_qty {
            return Err(FilterError::BelowMinQty);
        }
        if !(qty.get() % self.lot_size.get()).is_zero() {
            return Err(FilterError::QtyNotOnLot);
        }
        if !(price.get() % self.tick_size.get()).is_zero() {
            return Err(FilterError::PriceNotOnTick);
        }
        if self.notional(qty, price) < self.min_notional {
            return Err(FilterError::BelowMinNotional);
        }
        Ok(())
    }
}

impl Deref for Instrument {
    type Target = InstrumentSpec;
    fn deref(&self) -> &InstrumentSpec {
        &self.0
    }
}

impl TryFrom<InstrumentSpec> for Instrument {
    type Error = Error;
    fn try_from(spec: InstrumentSpec) -> Result<Self, Error> {
        Self::new(spec)
    }
}

impl From<Instrument> for InstrumentSpec {
    fn from(i: Instrument) -> Self {
        i.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Binance BTCUSDT spot filters: tick 0.01, step 0.00001, min notional 5.
    fn btcusdt_spec() -> InstrumentSpec {
        InstrumentSpec {
            id: "binance:BTCUSDT".parse().unwrap(),
            class: AssetClass::Spot,
            base: Asset::new("BTC").unwrap(),
            quote: Asset::new("USDT").unwrap(),
            tick_size: Price::new(dec!(0.01)),
            lot_size: Qty::new(dec!(0.00001)),
            min_qty: Qty::new(dec!(0.00001)),
            min_notional: Notional::new(dec!(5)),
        }
    }

    fn btcusdt() -> Instrument {
        Instrument::new(btcusdt_spec()).unwrap()
    }

    #[test]
    fn notional_is_qty_times_price() {
        assert_eq!(
            btcusdt().notional(Qty::new(dec!(0.5)), Price::new(dec!(60000.00))),
            Notional::new(dec!(30000))
        );
    }

    #[test]
    fn qty_for_then_round_gives_lot_multiple_not_exceeding_budget() {
        // 100 / 60000 = 0.0016666...; floor to 0.00001 lot = 0.00166 BTC,
        // which costs 99.60 USDT <= 100.
        let i = btcusdt();
        let raw = i
            .qty_for(Notional::new(dec!(100)), Price::new(dec!(60000)))
            .unwrap();
        let q = i.round_qty(raw);
        assert_eq!(q, Qty::new(dec!(0.00166)));
        assert_eq!(
            i.notional(q, Price::new(dec!(60000))),
            Notional::new(dec!(99.6))
        );
    }

    #[test]
    fn qty_for_rejects_zero_price() {
        assert_eq!(
            btcusdt().qty_for(Notional::new(dec!(100)), Price::ZERO),
            Err(FilterError::NonPositivePrice)
        );
    }

    #[test]
    fn round_qty_truncates_toward_zero() {
        let i = btcusdt();
        assert_eq!(
            i.round_qty(Qty::new(dec!(0.123456789))),
            Qty::new(dec!(0.12345))
        );
        assert_eq!(
            i.round_qty(Qty::new(dec!(-0.123456789))),
            Qty::new(dec!(-0.12345))
        );
        assert_eq!(i.round_qty(Qty::new(dec!(0.000009))), Qty::ZERO);
    }

    #[test]
    fn round_price_goes_passive() {
        let i = btcusdt();
        let p = Price::new(dec!(100.005));
        assert_eq!(i.round_price(p, Side::Buy), Price::new(dec!(100.00)));
        assert_eq!(i.round_price(p, Side::Sell), Price::new(dec!(100.01)));
        // Already on tick: unchanged either way.
        let on = Price::new(dec!(100.01));
        assert_eq!(i.round_price(on, Side::Buy), on);
        assert_eq!(i.round_price(on, Side::Sell), on);
    }

    #[test]
    fn check_order_filters() {
        let i = btcusdt();
        let p = Price::new(dec!(40000));
        assert_eq!(i.check_order(Qty::new(dec!(0.001)), p), Ok(()));
        assert_eq!(
            i.check_order(Qty::ZERO, p),
            Err(FilterError::NonPositiveQty)
        );
        assert_eq!(
            i.check_order(Qty::new(dec!(0.001)), Price::ZERO),
            Err(FilterError::NonPositivePrice)
        );
        assert_eq!(
            i.check_order(Qty::new(dec!(0.000001)), p),
            Err(FilterError::BelowMinQty)
        );
        assert_eq!(
            i.check_order(Qty::new(dec!(0.000015)), p),
            Err(FilterError::QtyNotOnLot)
        );
        assert_eq!(
            i.check_order(Qty::new(dec!(0.001)), Price::new(dec!(40000.005))),
            Err(FilterError::PriceNotOnTick)
        );
        // 0.0001 * 40000 = 4 USDT < 5 minimum.
        assert_eq!(
            i.check_order(Qty::new(dec!(0.0001)), p),
            Err(FilterError::BelowMinNotional)
        );
    }

    #[test]
    fn settlement_by_class() {
        assert_eq!(btcusdt().settlement(), Settlement::Instant);
        let eq = Instrument::new(InstrumentSpec {
            id: "alpaca:AAPL".parse().unwrap(),
            class: AssetClass::UsEquity,
            base: Asset::new("AAPL").unwrap(),
            quote: Asset::new("USD").unwrap(),
            ..btcusdt_spec()
        })
        .unwrap();
        assert_eq!(eq.settlement(), Settlement::TPlus(1));
    }

    #[test]
    fn new_rejects_bad_reference_data() {
        let zero_tick = InstrumentSpec {
            tick_size: Price::ZERO,
            ..btcusdt_spec()
        };
        assert!(Instrument::new(zero_tick).is_err());
        let zero_lot = InstrumentSpec {
            lot_size: Qty::ZERO,
            ..btcusdt_spec()
        };
        assert!(Instrument::new(zero_lot).is_err());
        let neg_min = InstrumentSpec {
            min_notional: Notional::new(dec!(-1)),
            ..btcusdt_spec()
        };
        assert!(Instrument::new(neg_min).is_err());
        let same = InstrumentSpec {
            quote: Asset::new("btc").unwrap(),
            ..btcusdt_spec()
        };
        assert!(Instrument::new(same).is_err());
    }

    #[test]
    fn deserialize_validates() {
        let ok = serde_json::to_string(&btcusdt()).unwrap();
        assert_eq!(serde_json::from_str::<Instrument>(&ok).unwrap(), btcusdt());
        let bad = ok.replace(r#""tick_size":"0.01""#, r#""tick_size":"0""#);
        assert!(serde_json::from_str::<Instrument>(&bad).is_err());
    }
}
