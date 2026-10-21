use thiserror::Error;

/// Storage failure. Never exposes backend types, so call sites stay
/// independent of SeaORM and of the database in use.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The database rejected or failed the operation (connection, SQL,
    /// constraint).
    #[error("database error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A stored row doesn't decode to a valid core value: unknown enum text,
    /// inexact decimal, or state the core constructors reject.
    #[error("corrupt row in {table}: {reason}")]
    Corrupt { table: &'static str, reason: String },
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
