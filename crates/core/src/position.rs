use crate::{InstrumentId, Notional, Price, Qty};
use serde::{Deserialize, Serialize};

/// Holding in one instrument, valued by average cost.
///
/// `avg_cost` is quote paid per unit held, fees included, so `realized_pnl`
/// is after costs. Spot positions never go negative (no shorting). Changes
/// only by applying a [`Fill`](crate::Fill); that accounting lands in a
/// separate change with owner-confirmed expected numbers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    instrument: InstrumentId,
    qty: Qty,
    avg_cost: Price,
    realized_pnl: Notional,
}

impl Position {
    /// No holding and no realized P&L yet.
    pub fn flat(instrument: InstrumentId) -> Self {
        Self {
            instrument,
            qty: Qty::ZERO,
            avg_cost: Price::ZERO,
            realized_pnl: Notional::ZERO,
        }
    }

    pub fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    /// Base units held.
    pub fn qty(&self) -> Qty {
        self.qty
    }

    /// Quote paid per unit held, fees included. Zero when flat.
    pub fn avg_cost(&self) -> Price {
        self.avg_cost
    }

    /// Profit closed out so far, after fees.
    pub fn realized_pnl(&self) -> Notional {
        self.realized_pnl
    }

    pub fn is_flat(&self) -> bool {
        self.qty.is_zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_position_is_all_zero() {
        let p = Position::flat("binance:BTCUSDT".parse().unwrap());
        assert!(p.is_flat());
        assert_eq!(p.avg_cost(), Price::ZERO);
        assert_eq!(p.realized_pnl(), Notional::ZERO);
    }
}
