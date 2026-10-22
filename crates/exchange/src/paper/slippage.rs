use super::Execution;
use bot_core::{Decimal, Instrument, Liquidity, Price, Side};

/// Turns a fill model's price into the price actually paid: worse for the
/// trader by the cost of crossing the spread and moving the book.
pub trait SlippageModel: Send + Sync {
    fn apply(&self, instrument: &Instrument, side: Side, exec: Execution) -> Price;
}

/// Fixed slippage in basis points on taker fills, rounded to the tick
/// against the trader (buys up, sells down), since real fills print on
/// ticks and rounding must not hand back the cost.
///
/// Maker fills get none: a limit order never fills worse than its limit,
/// so slipping it would model an impossible fill.
#[derive(Clone, Copy, Debug)]
pub struct BpsSlippage {
    rate: Decimal,
}

impl BpsSlippage {
    /// `bps` basis points (10 = 0.1%). `None` if negative: negative slippage
    /// would be a hidden edge.
    pub fn new(bps: Decimal) -> Option<Self> {
        (bps >= Decimal::ZERO).then(|| Self {
            rate: bps / Decimal::from(10_000),
        })
    }

    pub fn zero() -> Self {
        Self {
            rate: Decimal::ZERO,
        }
    }
}

impl SlippageModel for BpsSlippage {
    fn apply(&self, instrument: &Instrument, side: Side, exec: Execution) -> Price {
        if exec.liquidity == Liquidity::Maker || self.rate.is_zero() {
            return exec.price;
        }
        match side {
            Side::Buy => {
                let p = exec.price * (Decimal::ONE + self.rate);
                // round_price(.., Sell) rounds up: against a buyer.
                instrument.round_price(p, Side::Sell)
            }
            Side::Sell => {
                let p = exec.price * (Decimal::ONE - self.rate);
                instrument.round_price(p, Side::Buy)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::test_support::btcusdt;
    use rust_decimal_macros::dec;

    fn exec(price: Decimal, liquidity: Liquidity) -> Execution {
        Execution {
            price: Price::new(price),
            liquidity,
            at_open: true,
        }
    }

    #[test]
    fn ten_bps_against_the_trader_rounded_to_tick() {
        let s = BpsSlippage::new(dec!(10)).unwrap();
        let i = btcusdt();
        // 101 * 1.001 = 101.101 -> up to tick 101.11
        assert_eq!(
            s.apply(&i, Side::Buy, exec(dec!(101), Liquidity::Taker)),
            Price::new(dec!(101.11))
        );
        // 101 * 0.999 = 100.899 -> down to tick 100.89
        assert_eq!(
            s.apply(&i, Side::Sell, exec(dec!(101), Liquidity::Taker)),
            Price::new(dec!(100.89))
        );
    }

    #[test]
    fn maker_fills_are_not_slipped() {
        let s = BpsSlippage::new(dec!(10)).unwrap();
        assert_eq!(
            s.apply(&btcusdt(), Side::Buy, exec(dec!(97), Liquidity::Maker)),
            Price::new(dec!(97))
        );
    }

    #[test]
    fn negative_bps_rejected() {
        assert!(BpsSlippage::new(dec!(-1)).is_none());
    }
}
