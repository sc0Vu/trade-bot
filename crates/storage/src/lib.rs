//! Persistence: SeaORM entities and repository traits.
//!
//! Call sites see repositories only, never the backend, so switching
//! `db.url` from SQLite to MySQL needs no model changes.
