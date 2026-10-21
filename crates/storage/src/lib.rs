//! Persistence: repository traits and their SeaORM implementation.
//!
//! Call sites depend only on [`TradeRepository`] and [`HoldingRepository`],
//! which speak core types (`Fill`, `Position`). SeaORM entities are private
//! to this crate, so no caller can couple to a table layout or backend, and
//! switching `db.url` from `sqlite://` to `mysql://` needs no model changes.
//!
//! Money is stored as exact decimal TEXT, never as a `DECIMAL` column: on
//! SQLite, SeaORM maps `DECIMAL` to `real` and reads it through `f64`, which
//! breaks invariant 1. Reading TEXT back goes through `Decimal::from_str_exact`
//! and then the core constructors, so a corrupt row is an error, never a
//! silently wrong value.

mod decimal;
mod entity;
mod error;
mod repo;
mod sql;

pub use error::StorageError;
pub use repo::{HoldingRepository, TradeId, TradeRepository};
pub use sql::SqlStore;
