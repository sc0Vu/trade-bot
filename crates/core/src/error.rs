use thiserror::Error as ThisError;

/// Validation failure when building a core type. Messages name the broken
/// rule, never the money values involved, so they are safe to log.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum Error {
    /// Text is not an exact decimal (floats, exponents and overflow are rejected).
    #[error("invalid decimal: {0:?}")]
    InvalidDecimal(String),
}
