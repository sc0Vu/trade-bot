use bot_core::{Decimal, Fee, Instrument, Liquidity, Price, Qty, Side};

/// What the venue charges for one fill.
pub trait FeeModel: Send + Sync {
    fn fee(
        &self,
        instrument: &Instrument,
        side: Side,
        qty: Qty,
        price: Price,
        liquidity: Liquidity,
    ) -> Fee;
}

/// Which asset a [`RateFee`] is charged in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeAsset {
    /// The asset received: base on a buy, quote on a sell. Binance spot's
    /// default (no BNB discount).
    Received,
    /// Always quote.
    Quote,
}

/// Fee as a fraction of the traded amount, by liquidity. Not rounded: the
/// exact rate times the amount, so the backtest never under-charges by
/// rounding.
#[derive(Clone, Copy, Debug)]
pub struct RateFee {
    maker: Decimal,
    taker: Decimal,
    asset: FeeAsset,
}

impl RateFee {
    /// Rates as fractions (`0.001` = 0.1%). `None` unless both are in
    /// `[0, 1)`: a negative fee would be a hidden rebate edge, and a fee of
    /// 100% or more would consume the whole fill.
    pub fn new(maker: Decimal, taker: Decimal, asset: FeeAsset) -> Option<Self> {
        let ok = |r: Decimal| r >= Decimal::ZERO && r < Decimal::ONE;
        (ok(maker) && ok(taker)).then_some(Self {
            maker,
            taker,
            asset,
        })
    }

    pub fn zero() -> Self {
        Self {
            maker: Decimal::ZERO,
            taker: Decimal::ZERO,
            asset: FeeAsset::Quote,
        }
    }
}

impl FeeModel for RateFee {
    fn fee(
        &self,
        instrument: &Instrument,
        side: Side,
        qty: Qty,
        price: Price,
        liquidity: Liquidity,
    ) -> Fee {
        let rate = match liquidity {
            Liquidity::Maker => self.maker,
            Liquidity::Taker => self.taker,
        };
        let on_quote = || Fee::Quote(instrument.notional(qty, price) * rate);
        match (self.asset, side) {
            (FeeAsset::Received, Side::Buy) => Fee::Base(qty * rate),
            (FeeAsset::Received, Side::Sell) | (FeeAsset::Quote, _) => on_quote(),
        }
    }
}

impl Default for RateFee {
    fn default() -> Self {
        Self::zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::test_support::btcusdt;
    use bot_core::Notional;
    use rust_decimal_macros::dec;

    fn binance() -> RateFee {
        RateFee::new(dec!(0.001), dec!(0.001), FeeAsset::Received).unwrap()
    }

    #[test]
    fn received_asset_fee_like_binance() {
        let (i, q, p) = (btcusdt(), Qty::new(dec!(10)), Price::new(dec!(101)));
        // Buy 10 BTC: 0.1% of 10 = 0.01 BTC.
        assert_eq!(
            binance().fee(&i, Side::Buy, q, p, Liquidity::Taker),
            Fee::Base(Qty::new(dec!(0.01)))
        );
        // Sell 10 BTC at 101: 0.1% of 1010 = 1.01 USDT.
        assert_eq!(
            binance().fee(&i, Side::Sell, q, p, Liquidity::Taker),
            Fee::Quote(Notional::new(dec!(1.01)))
        );
    }

    #[test]
    fn quote_asset_and_maker_rate() {
        let f = RateFee::new(dec!(0.0002), dec!(0.001), FeeAsset::Quote).unwrap();
        let (i, q, p) = (btcusdt(), Qty::new(dec!(10)), Price::new(dec!(101)));
        // Maker 0.02% of 1010 = 0.202 USDT.
        assert_eq!(
            f.fee(&i, Side::Buy, q, p, Liquidity::Maker),
            Fee::Quote(Notional::new(dec!(0.202)))
        );
    }

    #[test]
    fn rates_must_be_in_unit_interval() {
        assert!(RateFee::new(dec!(-0.001), dec!(0.001), FeeAsset::Quote).is_none());
        assert!(RateFee::new(dec!(0.001), dec!(1), FeeAsset::Quote).is_none());
    }
}
