//! Adds what a restart needs to reconcile with the venue:
//! - `trades.venue_trade_id`: the venue's id for each fill, unique per
//!   instrument, so replaying the venue's trade history can't record a
//!   fill twice.
//! - `orders`: a journal of every order sent, written before it is sent,
//!   so a crash mid-submit still leaves a row to check against the venue.
//! - `bot_meta`: which network (testnet, demo, live) this database tracks,
//!   so fake-money and real-money books can't mix in one file.

use sea_orm_migration::{prelude::*, schema::*};

/// `venue:SYMBOL`, each part at most 32 chars (as in the first migration).
const INSTRUMENT_LEN: u32 = 65;
/// Binance's `newClientOrderId` limit.
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
                    .add_column(string_len_null(Trades::VenueTradeId, 64))
                    .to_owned(),
            )
            .await?;
        // NULLs (paper and pre-migration fills) never collide in a unique
        // index on SQLite or MySQL.
        manager
            .create_index(
                Index::create()
                    .name("idx_trades_venue_trade_id")
                    .table(Trades::Table)
                    .col(Trades::Instrument)
                    .col(Trades::VenueTradeId)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(Orders::Table)
                    .if_not_exists()
                    .col(string_len(Orders::ClientOrderId, CLIENT_ID_LEN).primary_key())
                    .col(string_len(Orders::Instrument, INSTRUMENT_LEN))
                    .col(string_len(Orders::Side, 4))
                    .col(string_len(Orders::Status, 8))
                    .col(big_integer_null(Orders::VenueOrderId))
                    .col(big_integer(Orders::CreatedAt))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_orders_status")
                    .table(Orders::Table)
                    .col(Orders::Status)
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(BotMeta::Table)
                    .if_not_exists()
                    .col(string_len(BotMeta::Key, 32).primary_key())
                    .col(string_len(BotMeta::Value, 64))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(BotMeta::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Orders::Table).to_owned())
            .await?;
        // SQLite refuses to drop a column an index still covers.
        manager
            .drop_index(
                Index::drop()
                    .name("idx_trades_venue_trade_id")
                    .table(Trades::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Trades::Table)
                    .drop_column(Trades::VenueTradeId)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Trades {
    Table,
    Instrument,
    VenueTradeId,
}

#[derive(DeriveIden)]
enum Orders {
    Table,
    ClientOrderId,
    Instrument,
    Side,
    Status,
    VenueOrderId,
    CreatedAt,
}

#[derive(DeriveIden)]
enum BotMeta {
    Table,
    Key,
    Value,
}
