use thiserror::Error as ThisError;

/// Validation failure when building a core type. Messages name the broken
/// rule, never the money values involved, so they are safe to log.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum Error {
    /// Identifier is empty, too long, or contains a character outside its
    /// allowed set.
    #[error("invalid {kind}: {value:?}")]
    InvalidId { kind: &'static str, value: String },
    /// Text is not an exact decimal (floats, exponents and overflow are rejected).
    #[error("invalid decimal: {0:?}")]
    InvalidDecimal(String),
    #[error("invalid instrument: {0}")]
    InvalidInstrument(&'static str),
}

/// Why an order would be rejected by the venue's filters. Checked by
/// `Instrument::check_order` before submit, identically in backtest and live,
/// so a backtest never fills an order the venue would refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ThisError)]
pub enum FilterError {
    #[error("quantity must be positive")]
    NonPositiveQty,
    #[error("price must be positive")]
    NonPositivePrice,
    #[error("quantity below instrument minimum")]
    BelowMinQty,
    #[error("quantity is not a multiple of lot size")]
    QtyNotOnLot,
    #[error("price is not a multiple of tick size")]
    PriceNotOnTick,
    #[error("notional below instrument minimum")]
    BelowMinNotional,
}
