use crate::{Asset, Error, InstrumentId, Notional, OrderId, Price, Qty, Side, Timestamp};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

/// Whether the fill added liquidity (maker, resting order) or took it
/// (taker). Decides the fee rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Maker,
    Taker,
}

/// Fee charged on one fill, in the asset the venue charged it in.
///
/// The asset matters for position math. Binance charges a buy's fee in base
/// (you receive less BTC), a sell's in quote (you receive less USDT), or in
/// BNB when that discount is on. A base fee shrinks the position; a quote
/// fee shrinks the cash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fee {
    /// In the instrument's base asset.
    Base(Qty),
    /// In the instrument's quote asset.
    Quote(Notional),
    /// In a third asset (e.g. BNB). v1 has no way to value it, so position
    /// accounting rejects it rather than silently ignoring the cost.
    Other { asset: Asset, amount: Qty },
}

impl Fee {
    fn is_negative(&self) -> bool {
        match self {
            Self::Base(q) | Self::Other { amount: q, .. } => q.is_negative(),
            Self::Quote(n) => n.is_negative(),
        }
    }
}

/// Unchecked fill fields. Turn them into a [`Fill`] with [`Fill::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FillSpec {
    pub order_id: OrderId,
    pub instrument: InstrumentId,
    pub side: Side,
    /// Base units executed in this fill (one order may fill in parts).
    pub qty: Qty,
    /// Execution price, slippage included.
    pub price: Price,
    pub fee: Fee,
    pub liquidity: Liquidity,
    pub ts: Timestamp,
}

/// One execution reported by a broker, paper or live. The only way a
/// position changes. Always has positive `qty` and `price` and a
/// non-negative fee.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "FillSpec", into = "FillSpec")]
pub struct Fill(FillSpec);

impl Fill {
    pub fn new(spec: FillSpec) -> Result<Self, Error> {
        if !spec.qty.is_positive() {
            return Err(Error::InvalidFill("qty must be positive"));
        }
        if !spec.price.is_positive() {
            return Err(Error::InvalidFill("price must be positive"));
        }
        if spec.fee.is_negative() {
            return Err(Error::InvalidFill("fee must be non-negative"));
        }
        Ok(Self(spec))
    }
}

impl Deref for Fill {
    type Target = FillSpec;
    fn deref(&self) -> &FillSpec {
        &self.0
    }
}

impl TryFrom<FillSpec> for Fill {
    type Error = Error;
    fn try_from(spec: FillSpec) -> Result<Self, Error> {
        Self::new(spec)
    }
}

impl From<Fill> for FillSpec {
    fn from(f: Fill) -> Self {
        f.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn spec() -> FillSpec {
        FillSpec {
            order_id: OrderId::new("bt-1").unwrap(),
            instrument: "binance:BTCUSDT".parse().unwrap(),
            side: Side::Buy,
            qty: Qty::new(dec!(0.001)),
            price: Price::new(dec!(60000)),
            fee: Fee::Base(Qty::new(dec!(0.000001))),
            liquidity: Liquidity::Taker,
            ts: Timestamp::from_millis(0),
        }
    }

    #[test]
    fn valid_fill() {
        assert!(Fill::new(spec()).is_ok());
    }

    #[test]
    fn rejects_bad_fills() {
        assert!(
            Fill::new(FillSpec {
                qty: Qty::ZERO,
                ..spec()
            })
            .is_err()
        );
        assert!(
            Fill::new(FillSpec {
                price: Price::ZERO,
                ..spec()
            })
            .is_err()
        );
        let rebate = Fee::Quote(Notional::new(dec!(-0.01)));
        assert!(
            Fill::new(FillSpec {
                fee: rebate,
                ..spec()
            })
            .is_err()
        );
    }
}
