//! SeaORM schema migrations, run via `sea-orm-cli migrate up|down|fresh`.
//!
//! Every schema change is a migration; the live database is never
//! hand-edited. Migrations must run unchanged on SQLite and MySQL, since
//! switching `db.url` is the whole MySQL story.
//!
//! Money columns are exact decimal TEXT, never `DECIMAL`: on SQLite, SeaORM
//! creates `DECIMAL` as `real(p,s)` and reads it back through `f64`, which
//! breaks invariant 1. TEXT round-trips exactly on every backend.

pub use sea_orm_migration::prelude::*;

mod m20261022_000001_create_trades_and_holdings;

/// Ordered list of every migration. Append only; never edit or reorder one
/// that has shipped.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(
            m20261022_000001_create_trades_and_holdings::Migration,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm_migration::sea_orm::Database;

    #[tokio::test]
    async fn up_creates_tables_and_down_removes_them() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = SchemaManager::new(&db);

        Migrator::up(&db, None).await.unwrap();
        assert!(schema.has_table("trades").await.unwrap());
        assert!(schema.has_table("holdings").await.unwrap());
        assert!(
            schema
                .has_index("trades", "idx_trades_instrument_ts")
                .await
                .unwrap()
        );

        Migrator::down(&db, None).await.unwrap();
        assert!(!schema.has_table("trades").await.unwrap());
        assert!(!schema.has_table("holdings").await.unwrap());
    }
}
