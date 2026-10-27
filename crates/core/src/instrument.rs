use crate::{
    Asset, Candle, Error, FilterError, InstrumentId, Notional, Price, Qty, Side, Timestamp,
};
use rust_decimal::Decimal;
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
    /// How far an order's price may stray from the recent average price.
    /// `None`: the venue has no such rule, or the data predates it (an
    /// older cache file); nothing is checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_band: Option<PriceBand>,
}

/// Binance `PERCENT_PRICE_BY_SIDE`: a buy's price must lie in
/// `[avg·bid_down, avg·bid_up]` and a sell's in `[avg·ask_down, avg·ask_up]`,
/// bounds included, where `avg` is the venue's average traded price over
/// the last `avg_minutes`. A fat-finger guard, but it binds normal
/// strategies too: a limit buy far above the market (BTCUSDT: more than
/// 1.2× the average) is refused even though it would fill at once.
///
/// It depends on the market, not only on reference data, so
/// [`Instrument::check_order`] can't apply it; callers pass the average
/// ([`Instrument::average_price`]) to [`Instrument::check_band`].
/// Multipliers serialize as strings, like money.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceBand {
    #[serde(with = "decimal_str")]
    pub bid_up: Decimal,
    #[serde(with = "decimal_str")]
    pub bid_down: Decimal,
    #[serde(with = "decimal_str")]
    pub ask_up: Decimal,
    #[serde(with = "decimal_str")]
    pub ask_down: Decimal,
    /// Minutes of trading the venue averages over (`avgPriceMins`).
    pub avg_minutes: u32,
}

/// Exact decimals as strings, never through a float (invariant 1's rule,
/// applied to the band's multipliers).
mod decimal_str {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Decimal, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(d)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
        let s = String::deserialize(d)?;
        Decimal::from_str_exact(&s).map_err(serde::de::Error::custom)
    }
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
        if let Some(b) = &spec.price_band {
            let positive = [b.bid_up, b.bid_down, b.ask_up, b.ask_down]
                .iter()
                .all(|m| m.is_sign_positive() && !m.is_zero());
            if !positive || b.bid_down > b.bid_up || b.ask_down > b.ask_up {
                return Err(Error::InvalidInstrument(
                    "price_band multipliers must be positive, down <= up",
                ));
            }
            if b.avg_minutes == 0 {
                return Err(Error::InvalidInstrument(
                    "price_band avg_minutes must be >= 1",
                ));
            }
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

    /// Price per unit when `qty` units are worth `notional`, inverse of
    /// [`notional`](Self::notional). Used for average cost; not rounded to
    /// tick, since an average is not an order price.
    pub fn price_for(&self, notional: Notional, qty: Qty) -> Result<Price, FilterError> {
        if !qty.is_positive() {
            return Err(FilterError::NonPositiveQty);
        }
        match self.class {
            AssetClass::Spot | AssetClass::UsEquity => Ok(Price::new(notional.get() / qty.get())),
        }
    }

    /// Whether a position in this instrument may go negative (a short).
    ///
    /// Asset-class policy, checked at runtime by
    /// [`Position::check_against`](crate::Position::check_against), not
    /// encoded in `Qty` or `Position`, so a signed (short, perp) position can
    /// reuse the same types in v2. False for every v1 class: spot can't
    /// short, and equity shorting needs margin, which v1 doesn't model.
    pub fn allows_short(&self) -> bool {
        match self.class {
            AssetClass::Spot | AssetClass::UsEquity => false,
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

    /// The price a market order on `side` is sized and checked at, given a
    /// `reference` such as the last close: rounded to tick against the
    /// trader (a buy up, so a budget buys fewer units; a sell down, so the
    /// notional check stays conservative). A market order sends no price,
    /// so the venue's tick rule doesn't apply to it, but a reference can be
    /// off today's tick in history fetched with today's filters (SOLUSDT
    /// closed at 1.8421 in 2021; its tick is now 0.01).
    pub fn market_price(&self, reference: Price, side: Side) -> Price {
        let against = match side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        self.round_price(reference, against)
    }

    /// `check_order` for a market order of `qty` on `side` at `reference`
    /// (see [`market_price`](Self::market_price)). What "could this holding
    /// be sold?" means: checking a raw off-tick close instead would call
    /// every holding unsellable.
    pub fn check_market(&self, qty: Qty, reference: Price, side: Side) -> Result<(), FilterError> {
        self.check_order(qty, self.market_price(reference, side))
    }

    /// The venue's average price for the band, estimated from closed
    /// candles (oldest first, ending at the current event `now`): total
    /// quote volume over total base volume of the candles closing within
    /// the last `avg_minutes`. `None` if there is no band, no candle in the
    /// window, or no volume. An estimate: the venue averages trades, and a
    /// candle longer than the window counts whole (1h bars vs a 5-minute
    /// window), so a price near a bound can still be refused by the venue.
    pub fn average_price<'a>(
        &self,
        history: impl DoubleEndedIterator<Item = &'a Candle>,
        now: Timestamp,
    ) -> Option<Price> {
        let band = self.price_band.as_ref()?;
        let window = i64::from(band.avg_minutes) * 60_000;
        let cutoff = now.as_millis().saturating_sub(window);
        let (mut base, mut quote) = (Qty::ZERO, Notional::ZERO);
        for c in history
            .rev()
            .take_while(|c| c.close_time.as_millis() > cutoff)
        {
            base += c.volume;
            quote += c.quote_volume;
        }
        self.price_for(quote, base).ok()
    }

    /// The prices the band allows on `side` around the venue's average
    /// price `avg`, both bounds included. `None` without a band.
    pub fn price_range(&self, side: Side, avg: Price) -> Option<(Price, Price)> {
        let b = self.price_band.as_ref()?;
        let (down, up) = match side {
            Side::Buy => (b.bid_down, b.bid_up),
            Side::Sell => (b.ask_down, b.ask_up),
        };
        Some((Price::new(avg.get() * down), Price::new(avg.get() * up)))
    }

    /// Checks a limit `price` on `side` against the band around `avg`.
    /// Always passes without a band.
    pub fn check_band(&self, price: Price, side: Side, avg: Price) -> Result<(), FilterError> {
        match self.price_range(side, avg) {
            Some((lo, hi)) if price < lo || price > hi => Err(FilterError::OutsidePriceBand),
            _ => Ok(()),
        }
    }

    /// Checks `qty` at `price` against every venue filter. Run on the final,
    /// rounded order; returns the first rule broken. The price band is
    /// market-relative: see [`check_band`](Self::check_band).
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
            price_band: None,
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
    fn price_for_inverts_notional() {
        // 300.3 USDT for 2 BTC = 150.15 per BTC.
        let i = btcusdt();
        assert_eq!(
            i.price_for(Notional::new(dec!(300.3)), Qty::new(dec!(2))),
            Ok(Price::new(dec!(150.15)))
        );
        assert_eq!(
            i.price_for(Notional::new(dec!(1)), Qty::ZERO),
            Err(FilterError::NonPositiveQty)
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

    /// 0.0001 BTC at an off-tick 49999.999: a buy is checked at 50000.00
    /// (notional 5, the minimum: passes), a sell at 49999.99 (4.999999:
    /// too small). The raw price fails the tick rule either way.
    #[test]
    fn market_checks_round_the_reference_against_the_trader() {
        let i = btcusdt();
        let (q, p) = (Qty::new(dec!(0.0001)), Price::new(dec!(49999.999)));
        assert_eq!(i.market_price(p, Side::Buy), Price::new(dec!(50000.00)));
        assert_eq!(i.market_price(p, Side::Sell), Price::new(dec!(49999.99)));
        assert_eq!(i.check_order(q, p), Err(FilterError::PriceNotOnTick));
        assert_eq!(i.check_market(q, p, Side::Buy), Ok(()));
        assert_eq!(
            i.check_market(q, p, Side::Sell),
            Err(FilterError::BelowMinNotional)
        );
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
    fn no_v1_class_allows_short() {
        assert!(!btcusdt().allows_short());
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
            price_band: None,
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

    /// Binance BTCUSDT's `PERCENT_PRICE_BY_SIDE` (mainnet and demo, Oct 2026).
    fn banded() -> Instrument {
        Instrument::new(InstrumentSpec {
            price_band: Some(PriceBand {
                bid_up: dec!(1.2),
                bid_down: dec!(0.5),
                ask_up: dec!(2),
                ask_down: dec!(0.8),
                avg_minutes: 5,
            }),
            ..btcusdt_spec()
        })
        .unwrap()
    }

    fn px(d: Decimal) -> Price {
        Price::new(d)
    }

    #[test]
    fn band_bounds_are_inclusive_per_side() {
        let i = banded();
        let avg = px(dec!(80000));
        assert_eq!(
            i.price_range(Side::Buy, avg),
            Some((px(dec!(40000)), px(dec!(96000))))
        );
        assert_eq!(
            i.price_range(Side::Sell, avg),
            Some((px(dec!(64000)), px(dec!(160000))))
        );
        let ok = |p: Decimal, side| i.check_band(px(p), side, avg);
        assert_eq!(ok(dec!(96000), Side::Buy), Ok(()));
        assert_eq!(
            ok(dec!(96000.01), Side::Buy),
            Err(FilterError::OutsidePriceBand)
        );
        assert_eq!(ok(dec!(40000), Side::Buy), Ok(()));
        assert_eq!(
            ok(dec!(39999.99), Side::Buy),
            Err(FilterError::OutsidePriceBand)
        );
        assert_eq!(ok(dec!(64000), Side::Sell), Ok(()));
        assert_eq!(
            ok(dec!(63999.99), Side::Sell),
            Err(FilterError::OutsidePriceBand)
        );
        assert_eq!(
            ok(dec!(160000.01), Side::Sell),
            Err(FilterError::OutsidePriceBand)
        );
        // No band, no rule.
        assert_eq!(btcusdt().price_range(Side::Buy, avg), None);
        assert_eq!(
            btcusdt().check_band(px(dec!(1000000)), Side::Buy, avg),
            Ok(())
        );
    }

    fn bar(close_min: i64, volume: Decimal, quote: Decimal) -> Candle {
        let close = close_min * 60_000;
        Candle::new(crate::CandleSpec {
            instrument: "binance:BTCUSDT".parse().unwrap(),
            interval: crate::Interval::M1,
            open_time: Timestamp::from_millis(close - 60_000),
            close_time: Timestamp::from_millis(close),
            open: px(dec!(80000)),
            high: px(dec!(90000)),
            low: px(dec!(70000)),
            close: px(dec!(80000)),
            volume: Qty::new(volume),
            quote_volume: Notional::new(quote),
        })
        .unwrap()
    }

    /// Bars closing within the last 5 minutes count; one closing exactly
    /// 5 minutes ago is outside, as is anything older.
    /// (80 000 + 243 000) / (1 + 3) = 80 750.
    #[test]
    fn average_price_is_window_vwap() {
        let i = banded();
        let bars = [
            bar(3, dec!(1), dec!(100000)),
            bar(5, dec!(10), dec!(1000000)),
            bar(6, dec!(1), dec!(80000)),
            bar(10, dec!(3), dec!(243000)),
        ];
        let now = Timestamp::from_millis(10 * 60_000);
        assert_eq!(i.average_price(bars.iter(), now), Some(px(dec!(80750))));
        assert_eq!(btcusdt().average_price(bars.iter(), now), None);
        let quiet = [bar(10, dec!(0), dec!(0))];
        assert_eq!(i.average_price(quiet.iter(), now), None);
        assert_eq!(i.average_price([].iter(), now), None);
    }

    #[test]
    fn bad_band_is_refused() {
        let with = |b: PriceBand| {
            Instrument::new(InstrumentSpec {
                price_band: Some(b),
                ..btcusdt_spec()
            })
        };
        let good = banded().price_band.unwrap();
        assert!(
            with(PriceBand {
                bid_down: dec!(1.3),
                ..good
            })
            .is_err()
        );
        assert!(
            with(PriceBand {
                ask_down: dec!(0),
                ..good
            })
            .is_err()
        );
        assert!(
            with(PriceBand {
                avg_minutes: 0,
                ..good
            })
            .is_err()
        );
    }

    /// The band rides along in the cache file as strings; a file written
    /// before it existed still loads, with no band.
    #[test]
    fn band_serializes_as_strings_and_is_optional() {
        let json = serde_json::to_string(&banded()).unwrap();
        assert!(json.contains(r#""bid_up":"1.2""#), "{json}");
        assert!(json.contains(r#""avg_minutes":5"#), "{json}");
        assert_eq!(serde_json::from_str::<Instrument>(&json).unwrap(), banded());
        let old = serde_json::to_string(&btcusdt()).unwrap();
        assert!(!old.contains("price_band"), "{old}");
        assert_eq!(serde_json::from_str::<Instrument>(&old).unwrap(), btcusdt());
        let float = json.replace(r#""bid_up":"1.2""#, r#""bid_up":1.2"#);
        assert!(serde_json::from_str::<Instrument>(&float).is_err());
    }
}
