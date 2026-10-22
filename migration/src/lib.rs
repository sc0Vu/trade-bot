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
mod m20261022_000002_create_orders_and_venue_trade_ids;
mod m20261022_000003_add_trade_fee_value;
mod m20261023_000004_create_strategy_state;

/// Ordered list of every migration. Append only; never edit or reorder one
/// that has shipped.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20261022_000001_create_trades_and_holdings::Migration),
            Box::new(m20261022_000002_create_orders_and_venue_trade_ids::Migration),
            Box::new(m20261022_000003_add_trade_fee_value::Migration),
            Box::new(m20261023_000004_create_strategy_state::Migration),
        ]
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
        assert!(schema.has_table("orders").await.unwrap());
        assert!(schema.has_table("bot_meta").await.unwrap());
        assert!(schema.has_column("trades", "venue_trade_id").await.unwrap());
        assert!(schema.has_column("trades", "fee_value").await.unwrap());
        assert!(schema.has_table("strategy_state").await.unwrap());
        for (table, index) in [
            ("trades", "idx_trades_instrument_ts"),
            ("trades", "idx_trades_venue_trade_id"),
            ("orders", "idx_orders_status"),
        ] {
            assert!(schema.has_index(table, index).await.unwrap(), "{index}");
        }

        Migrator::down(&db, Some(1)).await.unwrap();
        assert!(!schema.has_table("strategy_state").await.unwrap());
        Migrator::down(&db, Some(1)).await.unwrap();
        assert!(!schema.has_column("trades", "fee_value").await.unwrap());
        // Rolling back the second migration too restores the first's schema.
        Migrator::down(&db, Some(1)).await.unwrap();
        assert!(!schema.has_table("orders").await.unwrap());
        assert!(!schema.has_column("trades", "venue_trade_id").await.unwrap());
        assert!(schema.has_table("trades").await.unwrap());
        Migrator::up(&db, None).await.unwrap();

        Migrator::down(&db, None).await.unwrap();
        assert!(!schema.has_table("trades").await.unwrap());
        assert!(!schema.has_table("holdings").await.unwrap());
    }
}
