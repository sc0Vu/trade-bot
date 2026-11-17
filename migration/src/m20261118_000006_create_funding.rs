//! Creates `funding`: perpetual funding payments as the venue booked them
//! (Binance income `FUNDING_FEE`), one row per venue transaction, so a
//! restart's realized P&L still counts them. Each row is folded into the
//! instrument's holding (`realized_pnl`) in the same transaction it is
//! inserted. The amount is exact decimal TEXT like every money column.

use sea_orm_migration::{prelude::*, schema::*};

const MONEY_LEN: u32 = 40;
const INSTRUMENT_LEN: u32 = 65;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Funding::Table)
                    .if_not_exists()
                    .col(big_pk_auto(Funding::Id))
                    .col(string_len(Funding::Instrument, INSTRUMENT_LEN))
                    .col(string_len(Funding::VenueId, 64))
                    .col(string_len(Funding::Amount, MONEY_LEN))
                    .col(big_integer(Funding::Ts))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_funding_venue_id")
                    .table(Funding::Table)
                    .col(Funding::Instrument)
                    .col(Funding::VenueId)
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Funding::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Funding {
    Table,
    Id,
    Instrument,
    VenueId,
    Amount,
    Ts,
}
