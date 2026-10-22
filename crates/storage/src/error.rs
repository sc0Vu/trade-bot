use thiserror::Error;

/// Storage failure. Never exposes backend types, so call sites stay
/// independent of SeaORM and of the database in use.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The database rejected or failed the operation (connection, SQL,
    /// constraint).
    #[error("database error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Position accounting refused the fill (oversell, unsupported fee asset,
    /// wrong instrument). Nothing was written.
    #[error("fill rejected: {0}")]
    Rejected(#[source] bot_core::Error),
    /// A stored row doesn't decode to a valid core value: unknown enum text,
    /// inexact decimal, or state the core constructors reject.
    #[error("corrupt row in {table}: {reason}")]
    Corrupt { table: &'static str, reason: String },
    /// An order-journal update named an order that was never journaled.
    #[error("no journaled order {0}")]
    UnknownOrder(String),
    /// The database belongs to another network (say, a demo database
    /// opened for a live run). Mixing them would count fake fills as real
    /// holdings.
    #[error("database tracks network {found}, not {wanted}; use a separate db.url per network")]
    WrongNetwork { wanted: String, found: String },
}

impl StorageError {
    pub(crate) fn backend(e: sea_orm::DbErr) -> Self {
        Self::Backend(Box::new(e))
    }

    pub(crate) fn corrupt(table: &'static str, reason: impl ToString) -> Self {
        Self::Corrupt {
            table,
            reason: reason.to_string(),
        }
    }
}
