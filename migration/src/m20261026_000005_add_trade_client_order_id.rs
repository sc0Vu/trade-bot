//! Adds `trades.client_order_id`: the order a venue fill belongs to, as
//! the journal (`orders.client_order_id`) and the venue know it.
//! `trades.order_id` is the engine's id, which restarts at `o-1` every run,
//! so it can't tell one run's `o-2` from another's. NULL for paper fills
//! and for fills recorded before this migration (no exact link exists for
//! them, and a guessed one would be worse than none).

use sea_orm_migration::{prelude::*, schema::*};

/// Binance's `newClientOrderId` limit (as in the orders migration).
const CLIENT_ID_LEN: u32 = 36;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Trades::Table)
                    .add_column(string_len_null(Trades::ClientOrderId, CLIENT_ID_LEN))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_trades_client_order_id")
                    .table(Trades::Table)
                    .col(Trades::ClientOrderId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // SQLite can't drop an indexed column: index first.
        manager
            .drop_index(
                Index::drop()
                    .name("idx_trades_client_order_id")
                    .table(Trades::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Trades::Table)
                    .drop_column(Trades::ClientOrderId)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Trades {
    Table,
    ClientOrderId,
}
